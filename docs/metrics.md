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
| `two_bot_process_incarnation{part}` | Nonzero random process-lifetime identity in four unsigned 32-bit numeric parts (`0`, `1`, `2`, `3`), emitted with the counters in the same scrape; no PID, timestamp, build or identity-valued label. Stable across renders and new on process replacement. Optional for older containers; partial, duplicate, mislabeled or invalid parts invalidate the scrape. Used only to attribute gateway pairing windows, not as recovery or health evidence |
| `two_bot_gateway_latency_seconds` | Last heartbeat round-trip from Twilight's completed ACK sample |
| `two_bot_gateway_reconnects_total` | New HELLOs after the first HELLO in the running loop (successful transport reconnections, not failed dial attempts) |
| `two_bot_gateway_resumes_total` | Received RESUMED dispatches |
| `two_bot_gateway_disconnects_total` | Observed transport losses funnelled through the shard supervisor (reconnect failures, close frames, invalid sessions, cold-resume IDENTIFY). Every disconnect must pair with a later RESUME or fresh READY in the same window; an unpaired disconnect means the gateway never came back |
| `two_bot_gateway_missed_events_total` | Dispatches Discord assigned but this process never received (sequence gaps inside one session). Any nonzero increase over the watch window fails the zero-missed-events acceptance |
| `two_bot_gateway_rsvp_refused_total` | RSVP interactions refused at reception because the in-flight RSVP bound (32) was exhausted: no prepare task spawned and nothing entered the serial drain. The dispatch advances with no RSVP completion instead of wedging the funnel, checkpoint or dispatch worker |
| `two_bot_gateway_events_total{event}` | Received dispatches, including replays/duplicates, plus heartbeat ACKs and closes; fixed type allowlist, remainder `other` |
| `two_bot_presence_capture_total{outcome}` | Presence-update capture decisions on the hot path; `outcome` is `inserted` (first delivery stored a row), `duplicate` (same-key retry absorbed by the dedup key) or `rejected` (refused before the insert). Fixed three series from process start; no user, activity, application or free-text labels. Deliberately unwatched: no production callsite records this family yet (the event-store S3 producer is still cut over), so the rule lands with the producer; see the catalog |
| `two_bot_automod_decisions_total{outcome}` | Terminal automod inspection outcomes; `outcome` is `accepted`, `already_processed`, `dry_run`, `protected`, `deleted`, `warned`, `timed_out` or `sanction_refused`. Recorded once per committed `StoredOutcome` by the shared activation; replays and retained claims never count. Fixed eight series from process start; no message text, excerpts or user IDs become labels. The `automod_sanction_refused` rule tickets on any `sanction_refused` increase (first sample is baseline-only, restarts re-baseline, missing family is no window); the other seven outcomes are policy working as designed and stay quiet |
| `two_bot_handler_duration_seconds` | Cumulative histogram over nonduplicate dispatch parse/pipeline/durable commit, including failures; seconds |
| `two_bot_rest_requests_total{route,result}` | Executor HTTP sends, including retries; result `2xx`, `3xx`, `4xx`, `429`, `5xx` at response headers or `transport` (failure/cancellation/timeout before headers); later body failures do not hide 429/5xx |
| `two_bot_rest_guard_invalid_requests_total` | Shared guard observations of 401, 403 or 429 response headers, across guarded REST callers |
| `two_bot_rest_guard_rejected_requests_total` | Shared guard local refusals from `admit` or `check_now`, before any REST wire attempt; excludes admission timeout/cancellation |
| `two_bot_rest_guard_breaker_opens_total` | Invalid-request breaker transitions to open; the `rest_breaker_opens` rule tickets on any increase (first sample is baseline-only, restarts re-baseline, missing family is no window) |
| `two_bot_rest_guard_breaker_closes_total` | Invalid-request breaker transitions to closed; deliberately unwatched — a close cools an episode the opens ticket already named, so it stays resolution evidence (see the catalog), visible as the sustained-open ticket clearing plus the `discord_breaker_close` info log |
| `two_bot_rest_guard_global_pauses_total` | New global-pause episodes, not extensions or overlapping pending responses; the `rest_global_pauses` rule tickets on any increase (first sample is baseline-only, restarts re-baseline, missing family is no window) |
| `two_bot_rest_guard_invalid_requests_in_window` | Invalid response observations still inside the configured rolling window |
| `two_bot_rest_guard_breaker_open` | 1 when the invalid-request breaker is open, otherwise 0 |
| `two_bot_rest_guard_token_invalid` | 1 after a bot-authenticated 401, otherwise 0; fatal until process restart |
| `two_bot_rest_guard_pending_global_responses` | Global-header responses whose bodies are still unresolved |
| `two_bot_rest_guard_global_paused` | 1 while a global deadline remains or any global response body is unresolved, otherwise 0 |
| `two_bot_rest_guard_global_pause_remaining_seconds` | Finite nonnegative seconds until the longest global deadline; zero does not release unresolved response holds |
| `two_bot_db_pool_configured` | Whether gateway initialization has registered a pool |
| `two_bot_db_pool_connections` | Current pool size |
| `two_bot_db_pool_idle_connections` | Current idle connections |
| `two_bot_db_pool_max_connections` | Configured maximum |
| `two_bot_db_errors_total{op}` | Recorded storage-layer failures: send-admission SQL uses `op="admission"`, voice actor load uses `op="other"`; unknown instrumented operations also collapse to `other`. Not a database-wide total |
| `two_bot_containment_store_errors_total{op}` | Containment-store failures; `op` is `complete_incident` or `other`. A failed completion increments `complete_incident` once and marks the staff alert outcome-unknown because the terminal outcome could not be durably recorded. Fixed two series from process start; no guild, actor, incident or error text labels |
| `two_bot_send_admissions_total{outcome}` | Send-admission `admit()` decisions; `outcome` is `admitted`, `blocked`, `storage_error` (also counted in `two_bot_db_errors_total{op="admission"}`) or `other` |
| `two_bot_gateway_prefix_trigger_refused_total{reason}` | In-scope prefix candidates refused by the automod verdict before any trigger lookup; `reason` is `verdict` (contained, timed-out, uninspected, unrecorded or missing verdict) or `other`. The worker records candidates even when its capture-only gate prevents detached dispatch. Unmatched content (no trigger), scope mismatches, bot/webhook messages and disabled text-command gates never increment this family |
| `two_bot_job_runs_total{job,outcome}` | Completed attempts; outcome is `success` or `failure` (including returned errors, timeouts and isolated panics) |
| `two_bot_job_last_success_timestamp_seconds{job}` | Last successful completion time in Unix seconds; zero means no success recorded |
| `two_bot_job_consecutive_failures{job}` | Failed completions since the last success; resets to zero on success |
| `two_bot_job_last_error_class{job,class}` | One-hot class of the last failed completion; clears on success; fixed 18 × 7 = 126 series |
| `two_bot_voice_operations_total{op,outcome}` | Finished room create/move/delete outcomes; `op` is `create`, `move` or `delete`, `outcome` is `success`, `category_full`, `discord`, `persistence`, `cancelled` or `refused` (create admission refused: cap or rolling burst); retries and 429 backoffs are not outcomes |
| `two_bot_voice_reconcile_actions_total{action}` | Reconcile changes: newly queued deletes/ownership handoffs and actual access-state transitions; `action` is `delete_enqueued`, `suspended`, `resumed` or `succession_enqueued`. Repeated unchanged passes do not increment these counters |
| `two_bot_voice_dead_letters_total{action}` | Queue writes that exhausted `QUEUE_MAX_ATTEMPTS` (10); `action` is `create`, `move`, `delete`, `companion`, `ownership`, `kick`, `rename`, `limit` or `other` |
| `two_bot_voice_tracked_rooms` | Rooms tracked in memory; compare with live Discord channels for ghosts |
| `two_bot_voice_compensation_pending` | Tracked rooms awaiting compensating delete after a failed write |
| `two_bot_voice_orphans_total` | Untracked creator-channel orphans needing manual deletion after failed `/create` compensation |
| `two_bot_voice_joins_received_total` | Creator joins received by the guild actor; compare with create/move/refused outcomes — received persistently above handled means the actor is not processing joins |
| `two_bot_voice_join_refused_total{reason}` | Actor-received creator joins refused before queueing; `reason` is `halted`, `access`, `unknown_creator`, `stale_ticket`, `duplicate`, `name_blocked` or `other` |
| `two_bot_voice_inbox_overflow_dropped_total` | Fire-and-forget commands refused at the 256-entry actor inbox gate, including public ballot message references. One label-free increment per capacity refusal; accepted/lossless sends and closed-channel rollbacks do not count. Process-lifetime total survives actor retirement, resets on restart. Consumed by no alert rule; compare growth with queue age and dispatch progress when diagnosing overload |
| `two_bot_voice_queue_overflow_dropped_total` | New actions refused by opt-in `ActionQueue::try_enqueue` at 1,024 pending actions per guild. One label-free increment per refusal; at/past-cap coalescing does not count. Process-lifetime total survives queue retirement, resets on restart. All current production producers use uncapped must-admit `enqueue`, so this counter remains zero there: zero is not a healthy-drain signal or evidence of a production queue bound. Consumed by no alert rule |
| `two_bot_voice_worker_halted` | Guild-actor halt latch: 1 while any guild actor is halted by a store CredentialRefused or Discord 401 (only store halts auto-recover; 401s need a token fix + restart); aggregated over all current guild actors, so a healthy guild cannot hide another guild's halt; the `voice_worker_halted` rule pages while 1 (a valid 0 clears, missing holds) |
| `two_bot_voice_queue_pending{lane}` | Queued (not in-flight) voice actions, summed across all current guild actors per lane; `lane` is `urgent` or `deferred`; deliberately unwatched — depth without age is not a failure signal (bursts queue legitimately, no code-grounded bound exists), so it stays a manual complement to the `voice_queue_stall` ticket |
| `two_bot_voice_queue_oldest_age_seconds` | Age of the oldest queued voice action in seconds, the maximum across all current guild actors; 0 when every queue is empty; the `voice_queue_stall` rule tickets past 120 s (at or below clears, missing holds; a hung dispatch still climbs past the bound — the sibling recomputes the age from the snapshot stamp) |
| `two_bot_voice_live_not_ready_seconds` | Seconds a guild's live voice view has been not ready while the gateway is connected (READY or RESUMED handled), the maximum across all current guild actors, republished every actor tick; 0 when every view is ready or the gateway is down. While above 0 creator joins in that guild are dropped without an accept or refusal line and `two_bot_voice_joins_received_total` stays flat; every dispatch retries the cached snapshot, so a value that keeps climbing past 120 s needs a restart and a bug report; the `voice_live_not_ready` rule pages past 120 s (a hung dispatch still climbs past it — the sibling reads the live view directly) |
| `two_bot_voice_live_self_heal_total` | Live voice views republished from the gateway cache because they were not ready on a connected gateway; any increase is a fault the self-heal covered, worth a look in the container logs (`voice live view was not ready on a connected gateway`); the `voice_live_self_heal` rule tickets on any increase |
| `two_bot_voice_rest_in_flight_age_seconds` | Age of the oldest in-flight voice REST attempt in seconds; 0 when idle. The `voice_rest_stuck` rule tickets when the age reads above 60 s (six times the 10 s wire timeout) for 3 consecutive samples; a valid sample at or below 60 s resolves, missing/invalid samples break the consecutive-evidence streak but preserve an active firing ticket |
| `two_bot_voice_creator_map_generation` | Last merged creator-map generation; 0 before the first merge — a flat line across reconnects means the map stopped converging |
| `two_bot_voice_creator_map_age_seconds` | Seconds since the last successful creator-map merge, republished every actor tick; 0 when never merged — climbing past the 5 min refresh interval means merges stopped landing; the `voice_creator_map_stale` rule pages past 600 s once a merge has landed |
| `two_bot_voice_global_backoff_capped_total` | Global-backoff waits capped (returned RateLimited for tick-driven backoff) instead of parking the actor. The `voice_global_backoff_capped` rule tickets on any increase (first sample is baseline-only, restarts re-baseline, missing family is no window) |
| `two_bot_voice_dispatch_timeouts_total` | Actor dispatch turns that ran past the observe bound (run to completion; counted after the fact); the queue keeps its retries; the `voice_dispatch_timeouts` rule tickets on any increase (first sample is baseline-only, restarts re-baseline, missing family is no window) |
| `two_bot_voice_watchdog_stucks_total` | Watchdog firings: queued work with no dispatch progress inside the window; the `voice_watchdog_stucks` rule tickets on any increase (first sample is baseline-only, restarts re-baseline, missing family is no window) |
| `two_bot_voice_halt_recoveries_total` | Halt-latch recoveries via store-credential re-probe (halts caused by Discord 401 do not auto-recover); the `voice_halt_recoveries` rule tickets on any increase (first sample is baseline-only, restarts re-baseline, missing family is no window) |
| `two_bot_voice_actor_inbox_turn_bound` | Fair-scheduling bound: at most this many consecutive guild-actor inbox turns before a ready dispatch, timer or maintenance branch runs; 0 until the first actor publishes. Label-free gauge |
| `two_bot_voice_actor_inbox_turns_total` | Guild-actor inbox commands processed, all guilds. Compare with dispatch turns: inbox turns climbing while dispatch turns stay flat means the actor is draining churn without dispatching. Label-free counter |
| `two_bot_voice_actor_dispatch_turns_total` | Guild-actor dispatch turns that ran, whether or not the turn progressed. Label-free counter |
| `two_bot_voice_names_total{outcome}` | Automatic room-name decisions: a room created with its template name, each template render, and each queued rename sent or dropped at dispatch; `outcome` is `created_with_template`, `proposed`, `unchanged`, `waiting_for_name`, `blocked`, `channel_unseen`, `rename_sent`, `rename_room_gone`, `rename_superseded`, `rename_already_applied`, `rename_unseen`, `rename_no_access`, `rename_held`, `rename_duplicate`, `name_blocked` or `other` |
| `two_bot_voice_join_occupancy_total{occupancy}` | Organic VOICE_STATE_UPDATE joins by humans already present in the target channel when the join arrived, excluding the joiner; `occupancy` is `0`, `1`, `2`, `3`, `4`, `5`, `6` or `7+` (`7+` caps the tail). Leaves, same-channel updates, pre-READY frames and mechanical transitions (creator/generator target or previous channel: creator join, generator join, bot move echo) are not samples; joins into the session lobby or permanent protected channels still sample; no member, channel or time labels, so hourly curves use `increase(...[1h])` and there is no per-event log line. Deliberately unwatched: distribution telemetry for capacity curves, no bucket is a fault; see the catalog |
| `two_bot_voice_vote_kick_total{outcome}` | Vote-kick starts, refusals and terminal enforcements; `outcome` is `started`, a refusal code (`evidence_unavailable`, `not_a_room`, `initiator_not_occupant`, `target_not_occupant`, `self_target`, `protected_target`, `privileged_target`, `authority_unavailable`, `active_vote_exists`, `cooldown`, `initiator_limited`, `reused_vote_id`, `unknown_vote`, `wrong_vote_boundary`, `ineligible_voter`, `repeated_vote`, `invalid_time`), an enforcement code (`connect_denied_and_disconnected`, `connect_denied_target_absent`, `skipped_room_gone`, `skipped_target_protected`, `permission_missing`, `discord_error`, `gave_up`) or `other`; vote results (`passed`/`expired`/`cancelled`) are audit-only and never counted |
| `two_bot_event_voice_tick_total{outcome}` | Event-voice T-15 ticks that stayed TBD, by website reason; `outcome` is `opens_soon`, `rate_limited`, `perm_denied`, `no_category`, `unavailable` or `other`. Every TBD tick counts once, including backoff-skipped ticks; `Ready` ticks record nothing here — the ticker card extends this same series with its ready/create family |
| `two_bot_dispatch_drops_total{lane}` | Dispatch-lane saturation drops: every event refused because every attempted lane was full. `lane` is one of `messages`, `interactions`, `registry`, `privileged`, `busy`, `reactions` (see label allowlists below). The `reactions` lane additionally counts per-member fairness refusals: a reaction refused because its member already holds `PER_USER_IN_FLIGHT` reaction slots, even while the lane has free slots. A single-lane refusal counts its lane once; a privileged spill refused by both lanes counts both. Logs sample the first drop per 60 s per runtime, so bursts are O(1) lines with N counter increments. `dispatch_drops:<lane>` raises a ticket after growth in 3 consecutive completed keepalive sample windows; the first valid sample only establishes a baseline. A flat sample or counter reset clears the streak; missing/invalid samples break growth but never falsely resolve an active ticket. A single burst never pages. `reactions`-lane growth can reflect per-member fairness rather than an undersized lane. Dispatch refusals are not proof of gateway packet loss |
| `two_bot_raid_dropped_joins_total{reason}` | Joins not enqueued for raid observation; `reason` is exactly `queue_full` or `queue_closed`. Counts once per dropped join, not per alert. Both series start at zero and reset on process restart. No guild/member/error-text labels |
| `two_bot_join_risk_dropped_joins_total{reason}` | Joins not enqueued for risk scoring; the same two reasons and reset semantics as the raid counter. These observer drops do not undo gateway persistence. No automatic retry or new alert rule; see [queue triage](runbook-observability.md#raid-and-join-risk-queue-drops) |
| `two_bot_gateway_checkpoint_failures_total{stage}` | Failed gateway checkpoint commits from `apply_dispatch` and failed checkpoint clears: every failure stops the dispatch worker and is recorded on `operation`. `stage` is `pre_commit` (commit skipped after a funnel/leveling/acknowledgement failure) or `commit` (the durable store write itself failed); failure causes are never labels. Alert rule `gateway_checkpoint_failures` fires on any increase summed over every `stage` between consecutive keepalive samples (the first sample is baseline-only; the failing process lingers 75 s so a scrape lands before the restart resets the counter); unlike bursty dispatch drops, a single checkpoint failure stops the worker, so there is no benign-singleton case |
| `two_bot_gateway_drain_skips_total` | Auxiliary leveling drains skipped on SQLSTATE `42P01` (missing table) or `42501` (missing grant); once per failed drain, not per award. A skipped drain permits checkpoint advance and may lose XP; it does not promise replay. Other failures still hold the cursor, retaining the database code or bounded deadline reason. Label-free, zero on a fresh process; no SQLSTATE, member id or error text is a label. The `gateway_drain_skips` rule tickets on any increase (first sample is baseline-only, resets re-baseline, missing/invalid family is no window). This documents rule code, not alert deployment |
| `two_bot_drop_poison_total{guard}` | Poisoned locks recovered by drop guards instead of panicking inside `drop` (which aborts the process when the drop runs during unwinding). `guard` is one of `command_dispatch_guard`, `schedule_run`, `user_slot`, `in_flight_guard`, `ticket_supervisor`, `response_accounting` or `voice_connection_guard` (see label allowlists below). Each poisoned drop counts once and logs one `drop guard recovered a poisoned lock` warn with the guard name; the drop still releases its state from the recovered guard. A `ResponseAccounting` drop taking both the bare and global header paths counts twice, once per recovered lock. Any increase is a fault worth a look in the container logs. Poison stays sticky on the mutex, so later non-drop lock sites still panic until the broad lock-poison conversion lands; those panics unwind to task boundaries instead of aborting. Counted scope is the seven named guards, recovered only on their drop paths through drop-only variants; the shared live paths (`stop_tasks`, `finish_global`/`observe_bare_retry_after`, `VoiceRuntime::disconnect`) fail loud with no counter, so non-drop callers are never miscounted |
| `two_bot_internal_actions_total{family,outcome}` | Signed website-action receiver executions by bounded family and outcome. `family` is one of `announcement`, `event`, `settings`, `moderation`, `membership` or `other` (see label allowlists below). `outcome` is `executed` or the refusal class (`auth_failure`, `unknown_key`, `clock_skew`, `nonce_replay`, `rate_limit`, `unknown_action`, `action_disabled`, `malformed_body`, `conflict`, `upstream` or `internal`). Every request counts once; replays count on each serve. Refusal warn-summaries stay sampled; this counter is the alertable signal. Alert rule `receiver_refusals:<family>` fires when a family's refused outcomes rise in 3 consecutive keepalive samples (first sample and restarts clear the streak; one forged pre-auth probe in `other` stays silent) |
| `two_bot_moderation_enforcements_total{verb,outcome}` | Moderation enforcement terminal outcomes. `verb` is one of `ban`, `tempban`, `kick`, `timeout`, `warn`, `purge`, `slowmode`, `lockdown` or `unlock` (see label allowlists below). `outcome` is `success`, `refused`, `discord_error`, `storage_error` or `other`. Each terminal member/channel enforcement counts once; retries and rate-limit backoffs are not outcomes. The `moderation_enforcement_failures` rule tickets on any summed `discord_error`/`storage_error` increase (first sample is baseline-only, restarts re-baseline, missing family is no window); `success`, `refused` and `other` stay quiet |
| `two_bot_community_facts_drain_failures_total{reason}` | Warn-and-continue community-facts drain failures in the gateway dispatch worker. `reason` is `error` (the facts writer returned an error) or `timeout` (the drain missed the dispatch deadline), or `other` (see label allowlists below). Each failed dispatch counts once; a committed drain counts nothing. The worker never stalls on this, and the scorecard fails closed on missing coverage. Alert rule `community_facts_drain_failures` raises a ticket after summed growth in 3 consecutive completed keepalive sample windows; the first valid sample only establishes a baseline. A flat sample or counter reset clears the streak; missing/invalid samples break growth but never falsely resolve an active ticket. A single failed drain never pages |
| `two_bot_retention_replay_needed_total` | Skipped retention purges needing Operator replay: one per real guild removal whose purge-queue write hit the schema gap. Successful queue writes and rejoin-cancel skips never increment it. Label-free counter; resets on process restart, so pair it with the error log line. The `retention_replay_needed` rule tickets on any increase (first sample is baseline-only, restarts re-baseline, missing family is no window). No guild id, SQLSTATE or error text ever becomes a label |
| `two_bot_ticket_component_drops_total` | Ticket component interactions refused before spawning because the runtime held `MAX_TICKET_COMPONENT_IN_FLIGHT` (16) executions or the member held the per-member cap (3). One increment per refusal; admitted work never increments it. The refused interaction gets one ephemeral busy acknowledgement on the shared busy lane (or silence when that lane is also full); no ticket row, Discord effect beyond that callback, or recovery promise is produced for work never admitted. Label-free counter; resets on process restart, so pair it with the sampled `ticket_component_admission_saturated` warn line. No member id, token or interaction id ever becomes a label. Consumed by no alert rule; steady growth under load is expected backpressure, not paging |
| `two_bot_audit_delivery_halt` | Audit mirror delivery-halt state for staleness evaluation: `1` while the persistent kill switch is engaged, `0` once cleared. Label-free gauge set once per readable `audit_retry` sweep from the store's `delivery_halt` read; an unreadable halt keeps the last reported state, and a halted sweep still counts as a job `success` (the sweep ran; it claimed nothing by design). The `audit_delivery_halt` rule tickets after 3 consecutive valid engaged samples (~2 minutes from first to third at the default cadence); `0` recovers once. A failed `audit_retry` attempt vetoes both transitions because the gauge may be retained from an earlier readable sweep. Missing/invalid samples break the streak, not an active ticket. Production alert forwarding stays off. No actor ID, timestamp, error text or row content ever reaches exposition |
| `two_bot_event_store_consumer_lag_seconds` | Event-store consumer lag in seconds (`received_at` minus `event_time` for the newest observed capture row); `0` when unobserved. Label-free gauge; S1 exposes the series and its setter contract (last writer wins; non-finite or negative input clears to `0` so exposition stays a finite non-negative duration). The periodic store-read caller lands with the consumer slice, so until then the series reads `0` (unobserved), not "no lag". Alert rule `event_store_consumer_lag` tickets on sustained lag above 60 s (see below). No message, channel, user, dedup-key, content or body label ever reaches exposition |
| `two_bot_eventsub_received_total` | Verified-shape EventSub deliveries accepted for signature verification, including tampered signatures. Requests rejected before body intake are not deliveries; label-free counter. The `eventsub_receive_gap` rule tickets when received exceeds the summed classified kinds for 3 consecutive samples; a closed gap resolves, missing/invalid samples break the consecutive-evidence streak but preserve an active firing ticket |
| `two_bot_eventsub_classified_total{kind}` | EventSub deliveries classified by fixed `kind`: `challenge`, `notification`, `revocation`, `tampered`, `ignored`, one snake_case label per `EventSubError` variant (`invalid_secret`, `invalid_header`, `body_too_large`, `bad_signature`, `invalid_timestamp`, `stale_timestamp`, `invalid_body`, `duplicate_keys`, `wrong_subscription`, `invalid_challenge`, `invalid_callback`, `invalid_broadcaster_id`, `too_many_broadcasters`), or trailing `other`. Classify errors record their variant label (`wrong_subscription`, `invalid_body`, `duplicate_keys` and `invalid_challenge` for the deterministic arms; future variants record theirs); `ignored` covers unknown message types and non-signature verification failures; `other` absorbs unknown label strings. Only `BadSignature` is `tampered`. `eventsub_tampered` pages when the `tampered` series with exactly that label rises in 3 consecutive samples. No IDs, tokens, headers, body text or secrets become labels |
| `two_bot_eventsub_posts_total{outcome}` | Go-live post outcomes: `attempted`, `succeeded`, `failed` or `other`. Counted by the receiver only for deliveries that reach the sender through the durable ledger flow; boot injects no delivery dependencies yet, so these remain zero until channel/invite provisioning lands; no IDs, content or errors become labels |
| `two_bot_eventsub_reconcile_passes_total{outcome}` | EventSub reconcile passes by fixed `outcome`: `healthy` (every desired broadcaster holds a healthy subscription afterwards), `pending` (re-created but not all verified yet) or `failed` (an action failed or a healthy subscription is missing, including listing failures). Exactly one sample per concluded pass that reaches Helix; parked or refused startup passes (flag dark, invalid config, empty allowlist) never reach Helix and record no sample — they are log-only, as are passes interrupted by shutdown (which still record the mutations they already made in actions). Alert `eventsub_reconcile_failed` tickets when `failed` rises between keepalive samples (first sample is baseline-only; see the alert table below) |
| `two_bot_eventsub_reconcile_actions_total{action}` | Successful EventSub reconcile mutations by fixed `action`: `create` (2xx creates) or `delete` (2xx deletes, including already-gone deletes). 409 already-exists confirmations fold into the pass outcome, not into actions; per-subscription labels never reach exposition. Deliberately unwatched: successful mutations are the healthy signal, failures ticket through the failed-pass rule; see the catalog |
| `two_bot_eventsub_reconcile_errors_total{error_class}` | EventSub reconcile failure sites by the same fixed `error_class` strings as the `error_class` log field: `reconcile_action_failed` (once per failed plan action), `reconcile_prior_failed` (once per pass that found failed subscriptions), `reconcile_unhealthy` (once per non-converged pass) or `reconcile_failed` (listing or plan failures), plus `other` for unknown classes. No status code, body, broadcaster, subscription id or secret ever becomes a label. The `eventsub_reconcile_prior_failed` rule tickets on any `reconcile_prior_failed` increase after a healthy or pending pass completes with no failed-pass growth in the window (mid-pass scrapes retain all baselines; first samples and any counter reset re-baseline silently; a missing or invalid finding or pass-outcome row is no window); the other sites pair with a failed pass in the same run, so the failed-pass ticket already names that window |
| `two_bot_feed_fetches_total{outcome}` | Feed fetch attempts by bounded outcome: `success` (2xx fetch), `invalid_source` (source refused before dialling), `refused_redirect` (redirect refused by policy) or `failure` (every other failure, including non-2xx HTTP). One sample per fetch; per-outcome, never per-feed, so no source URL, host or feed id reaches a label. Deliberately unwatched: every non-success attempt pairs 1:1 with an errors increment, so the errors sum carries the same signal with finer reason codes; see the catalog |
| `two_bot_feed_fetch_errors_total{error_class}` | Failed feed fetches by bounded error class: `invalid_source`, `non_public_address`, `redirect_refused`, `too_large`, `invalid_encoding`, `unsupported_content_type`, `unsupported_encoding`, `resolve`, `transport`, `deadline`, `http_error` (completed non-2xx fetch) or `other`. One sample per failed fetch alongside `two_bot_feed_fetches_total`; successes record nothing here. No URL, host, header or body bytes reach a label. The `feed_fetch_failures` rule tickets on any summed increase (first sample is baseline-only, restarts re-baseline, missing family is no window) |
| `two_bot_ticket_lifecycle_total{event,outcome}` | Terminal ticket lifecycle events by bounded `event` and `outcome` (see label allowlists below). Retire, abandon, successful claim and successful close count once each alongside their `ticket_lifecycle` log line; the purge pass counts one per deleted transcript body (not one per pass) and reports a `db_cause` outcome instead of `deleted` when its storage write fails. No member text, channel name, ticket id or error string ever becomes a label. No alert rule consumes this family yet; read it alongside the lifecycle log lines in [ticket-lifecycle-observability.md](ticket-lifecycle-observability.md) |

## Voice overload bounds

The actor inbox refuses ordinary fire-and-forget sends at 256 outstanding
entries. Reply-bearing requests, acknowledged naming/access saves, edits to
existing creator rows and the startup display-name seed bypass this admission
gate; the limit is not a hard memory bound for all actor traffic. New-creator
notifications still use gated admission and can converge by periodic refresh
or first-join read-through. Saved edits to an existing creator's privacy must
reach an already-loaded worker before it plans the next room.

During initialization, up to 512 ordinary commands wait in arrival order;
overflow sheds the oldest ordinary command. Saved naming/access settings, each
creator channel's latest saved row and the latest startup display-name seed
are retained outside that eviction budget, coalesced by state key. They survive
failed attempts and backoff, while failed-attempt reply waiters close promptly.
This retains at most three guild-wide state entries plus one per distinct
creator channel, in addition to the ordinary FIFO; seed contents scale with the
guild snapshot. It is not a fixed total-memory bound. Init-buffer shedding is
not an inbox admission refusal and does not increment the inbox overflow counter.

The action-queue cap is opt-in: all current production producers still use
must-admit `enqueue`, not bounded `try_enqueue`. A stalled production action
drain can therefore grow cumulatively; zero overflow refusals do not prove
healthy drainage. Compare queue depth, oldest age and dispatch progress.

## REST admission guard

The eleven `two_bot_rest_guard_*` series have constant names and **no labels**.
The private scrape samples `ratelimit_guard::process_guard()`, the same instance
used by production REST executors and readiness, not a new guard or mirrored
counter registry. Fresh/unconfigured guard state renders all eleven series at
zero; counters reset on process restart. There is no reset/control endpoint.
Snapshot sampling uses the guard's existing rolling-window refresh, without
changing thresholds, clocks, admission decisions or readiness policy.

Invalid-response counts cover HTTP 401, 403 and 429 headers across guarded REST
callers, not just executor sends. Rejected counts include only refusals from
`admit` or `check_now` (breaker, invalid bot token or late global pause), not
admission timeouts or cancellations. These refusals and pre-wire admission
wait timeouts happen before an HTTP send, so they are **not**
`two_bot_rest_requests_total{route,result}` outcomes; they must not be added to
that family's denominator as transport failures.

Remaining duration is finite, nonnegative seconds, including fractional seconds.
It can be zero while admission remains globally paused: unresolved global-header
response bodies still hold admission until settled or dropped. Read remaining
duration together with `global_paused` and `pending_global_responses`, not as a
standalone readiness signal. The latch, invalid/refused and pending families
page through [REST token invalid](runbook.md#alert-rest-token-invalid), [REST
refusal surge](runbook.md#alert-rest-refusal-surge) and [REST global responses
stuck](runbook.md#alert-rest-global-responses-stuck); the breaker-opens and
global-pause counters ticket through `rest_breaker_opens` and
`rest_global_pauses` below. Breaker closes, the in-window counter and the
paused/remaining gauges have no alert rule (a live deadline is healthy
backpressure; see the stuck rule's runbook). The export leaves the existing
Worker authentication gate and 64 KiB scrape cap intact.

## Job coverage and outcomes

The supervisor records all three job metrics centrally after each completed
attempt. Individual periodic jobs need no instrumentation. The current scheduled
labels are `counter`, `rank`, `scheduled_events`, `presence_probe`,
`community_scorecard`, `inactivity`, `audit_retry`, `self_role_recovery`, `feeds`,
`member_unban_sweep`, `automod_expiry`, `lfg_sweep`, `event_voice`, `settings` and `scheduled_messages`
(the two 15 s tickers). The community, audit, recovery, feed, unban-sweep,
automod-expiry, LFG lifecycle, event-voice and ticker names may be parked by configuration.
All allowlisted series are exposed from process startup at zero, even before the
first run. A zero success timestamp does not distinguish a parked, never-started,
still-running or always-failing job; use `/readyz` job status for that distinction.

Starts, skipped busy deadlines and shutdown cancellation are not completed
outcomes. A returned `Ok(())` is a success even when the job is a gated/no-op tick,
including a feed inner poll skipped by the live interval gate; each such success
resets that job's consecutive-failure streak. These counters measure scheduler
health, not business events or changed rows.
Failures preserve the previous success timestamp. The supervisor's `/readyz`
status uses milliseconds; metrics convert the same completion sample to seconds.
Errors and panic payloads are never labels.

The fixed job allowlist also retains `session_checkpoint`, `invite_snapshot` and
`other`. `session_checkpoint` records successful durable gateway commits through
`job_success`, independently of the periodic supervisor; its success counter is
not a scheduled-job count and its failures are not instrumented here.
`invite_snapshot` remains zero until a real caller records a completion. Unknown
job names share the `other` counters, timestamp and failure streak; an unknown
success resets that shared streak, and `other` is now an unknown-name-only
fallback: every registered producer has its own label.
`feeds`, `self_role_recovery`, `member_unban_sweep`, `automod_expiry` and `lfg_sweep` have
independent counters, timestamps and streaks: success in any one cannot clear
failures or change last-success in another or in `other`. Add new scheduled
names to the compile-time allowlist, never to a dynamic label map. The
supervisor fixture checks the current website/community, audit,
scheduled-message, feed, self-role recovery, unban-sweep, automod-expiry and
LFG lifecycle names against that allowlist.

No scheduler or registration gate changes are implied by these labels. Feed
supervisor cadence follows the live interval (boot default 300 s, range
60–86,400 s): a stored change reschedules the next tick to last-run + new
interval and the per-tick schedule gate mirrors that slot, so ticks normally
land only when a poll is due. A skipped inner poll (a gate refusal on an
overlapping or not-yet-due tick) still returns success and resets the `feeds`
consecutive-failure streak, so a streak can under-report while such refusals
interleave with failures. Self-role recovery runs every 30 s with 0–5 s startup jitter
and a 25 s timeout; a successful sweep need not settle every pending repair.
The member unban sweep runs every 30 s (`UNBAN_SWEEP_INTERVAL_SECONDS`) with
startup jitter and a 180 s attempt timeout; one attempt may outlive its cadence,
and a successful sweep need not have completed any unban. Automod expiry runs
every 60 s (`EXPIRY_CADENCE`) with startup jitter and a 5 s timeout; a gateway
without built activation is a no-op success, and the job only does real expiry
work when `TWO_AUTOMOD=1`. No job's last-success timestamp is business-delivery
proof.
Executor calls already record REST metrics wherever the executor is used; other
REST clients are not silently claimed as covered.

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
  gateway event or job completion. Keep HELP descriptions concise: the full
  exposition must fit the consumers' 64 KiB body cap, not only the series-count
  budget. A regression saturates EventSub, pool and REST-guard samples and
  reserves 1 KiB for other value growth; it does not bound every possible
  combination of process counters at their maxima.

## Label allowlists (stable)

Renaming a metric, label, or allowlisted value breaks scrapers; new values
are added to these compile-time lists in `crates/core/src/metrics.rs`, never
as dynamic labels.

- `two_bot_eventsub_classified_total{kind}` — `kind` is one of
  `challenge`, `notification`, `revocation`, `tampered`, `ignored`, the thirteen
  `EventSubError` snake_case labels (`invalid_secret`, `invalid_header`,
  `body_too_large`, `bad_signature`, `invalid_timestamp`, `stale_timestamp`,
  `invalid_body`, `duplicate_keys`, `wrong_subscription`, `invalid_challenge`,
  `invalid_callback`, `invalid_broadcaster_id`, `too_many_broadcasters`), or
  trailing `other`.
- `two_bot_eventsub_posts_total{outcome}` — `outcome` is one of `attempted`,
  `succeeded`, `failed` or `other`.
- `two_bot_eventsub_reconcile_passes_total{outcome}` — `outcome` is one of
  `healthy`, `pending` or `failed`; unrecognised outcomes collapse to `failed`.
- `two_bot_eventsub_reconcile_actions_total{action}` — `action` is `create` or
  `delete`.
- `two_bot_eventsub_reconcile_errors_total{error_class}` — `error_class` is one
  of `reconcile_action_failed`, `reconcile_prior_failed`, `reconcile_unhealthy`,
  `reconcile_failed` or `other`.
- `two_bot_gateway_events_total{event}` — `event` is one of `READY`,
  `RESUMED`, `GUILD_CREATE`, `GUILD_DELETE`, `GUILD_UPDATE`,
  `GUILD_MEMBER_ADD`, `GUILD_MEMBER_REMOVE`, `GUILD_MEMBER_UPDATE`,
  `MESSAGE_CREATE`, `MESSAGE_UPDATE`, `MESSAGE_DELETE`,
  `MESSAGE_REACTION_ADD`, `MESSAGE_REACTION_REMOVE`,
  `VOICE_STATE_UPDATE`, `PRESENCE_UPDATE`, `INVITE_CREATE`, `INVITE_DELETE`,
  `INTERACTION_CREATE`, `HEARTBEAT_ACK`, `GATEWAY_CLOSE`, `other`.
- `two_bot_presence_capture_total{outcome}` — `outcome` is one of `inserted`,
  `duplicate`, `rejected`. Exactly three series; unrecognised outcomes collapse
  to `rejected`, never to a new series.
- `two_bot_automod_decisions_total{outcome}` — `outcome` is one of
  `accepted`, `already_processed`, `dry_run`, `protected`, `deleted`,
  `warned`, `timed_out` or `sanction_refused`
  (`crates/core/src/metrics.rs` `AUTOMOD_DECISION_OUTCOMES`, mirroring
  `automod_runtime::CompletionKind::as_str`). Recorded once per committed
  `StoredOutcome` in `crates/discord/src/automod_activation.rs` (`settle`);
  replays, retained claims, bypassed and unavailable inspections never count.
  No message text, excerpts or user IDs become labels; unrecognised outcomes
  collapse to `sanction_refused`, never to a new series.
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
  `PUT /channels/:channel/voice-status`,
  `PUT /channels/:channel/permissions/:overwrite`,
  `POST /channels/:channel/messages/bulk-delete`,
  `PUT /guilds/:guild/members/:member/roles/:role`,
  `DELETE /guilds/:guild/members/:member/roles/:role`,
  `POST /guilds/:guild/scheduled-events`,
  `PATCH /guilds/:guild/scheduled-events/:event`,
  `DELETE /guilds/:guild/scheduled-events/:event`,
  `POST /guilds/:guild/channels`, `DELETE /channels/:channel`,
  `GET /guilds/:guild/members/:member`, `GET /users/@me`,
  `PATCH /webhooks/:application/:token/messages/@original`, `other`).
- `two_bot_job_runs_total{job,outcome}`,
  `two_bot_job_last_success_timestamp_seconds{job}` and
  `two_bot_job_consecutive_failures{job}` — `job` is one of
  `invite_snapshot`, `session_checkpoint`, `counter`, `rank`,
  `scheduled_events`, `settings`, `presence_probe`, `community_scorecard`,
  `inactivity`, `audit_retry`, `scheduled_messages`, `feeds`, `self_role_recovery`,
  `member_unban_sweep`, `automod_expiry`, `lfg_sweep`, `event_voice`, `other`; `outcome` is `success` or `failure`.
  `session_checkpoint` records successful durable gateway commits; zero means
  never run. `audit_retry` is the audit supervisor's 30 s retry sweep.
  `member_unban_sweep` is the 30 s unban sweep. `automod_expiry` is the 60 s
  repeat-history expiry tick (real work only when `TWO_AUTOMOD=1`).
  `lfg_sweep` is the 60 s bounded LFG lifecycle pass (at most 20 candidates,
  announcements and boot identity gates required); row failures continue the
  batch with a warning. See [LFG lifecycle](lfg-port.md#bounded-lifecycle-maintenance).
  `event_voice` is the 60 s event voice-channel catch-up ticker (create window,
  reconcile and per-route 429 pause); it is parked with the other website jobs.
  `settings` is the DB-only 15 s `guild_settings` version poll.
- `two_bot_job_last_error_class{job,class}` — `job` uses the list above;
  `class` is one of `database`, `rest`, `configuration`, `timeout`, `panic`,
  `feed` or `recovery_required` (the fixed `ErrorClass` variants). Each of the
  18 jobs has one series per class (126 total); exactly the latest failure's class
  is `1`, and every class is `0` before a failure or after success. Unknown job
  names fold into `other`; class labels are fixed enum names, never error text.
- `two_bot_voice_operations_total{op,outcome}` — `op` is `create`, `move`
  or `delete`; `outcome` is `success`, `category_full`, `discord`,
  `persistence`, `cancelled` or `refused` (create admission refused the join). `Rejected` status/code values never become
  labels; all store variants share `persistence`. Post-move respace repairs
  are cosmetic and never increment these counters; each repair emits only a
  `voice_event="voice_respace"` log line.
- `two_bot_voice_reconcile_actions_total{action}` — `action` is
  `delete_enqueued`, `suspended`, `resumed` or `succession_enqueued`.
  `suspended` counts active-to-suspended transitions and `resumed` counts
  suspended-to-active transitions, including a suspended room found absent.
  Delete and succession counts include only newly queued work. A stable room
  contributes zero; `voice_reconcile` stays quiet when all four counts are zero.
- `two_bot_voice_dead_letters_total{action}` — `action` is `create`, `move`,
  `delete`, `companion`, `ownership`, `kick`, `rename`, `limit` or `other`.
- `two_bot_voice_names_total{outcome}` — `outcome` is `created_with_template`, `proposed`, `unchanged`, `waiting_for_name`, `blocked`, `channel_unseen`, `rename_sent`, `rename_room_gone`, `rename_superseded`, `rename_already_applied`, `rename_unseen`, `rename_no_access`, `rename_held`, `rename_duplicate`, `name_blocked` or `other`
  (`crates/core/src/metrics.rs` `VOICE_NAME_OUTCOMES`). `name_blocked` counts
  dispatch-time rename or custom-override filter refusals; `blocked` remains
  the template-render filter outcome. The former `rename_stale` outcome is split into `rename_room_gone` (no tracked room),
  `rename_superseded` (the queued name is no longer desired), and
  `rename_already_applied` (the live channel already has that name). These
  skips send no HTTP request; dashboards using the old label should sum the
  three replacement outcomes.
- `two_bot_voice_vote_kick_total{outcome}` — `outcome` is `started`,
  `evidence_unavailable`, `not_a_room`, `initiator_not_occupant`,
  `target_not_occupant`, `self_target`, `protected_target`,
  `privileged_target`, `authority_unavailable`, `active_vote_exists`,
  `cooldown`, `initiator_limited`, `reused_vote_id`, `unknown_vote`,
  `wrong_vote_boundary`, `ineligible_voter`, `repeated_vote`, `invalid_time`,
  `connect_denied_and_disconnected`, `connect_denied_target_absent`,
  `skipped_room_gone`, `skipped_target_protected`, `permission_missing`,
  `discord_error`, `gave_up` or `other` (`crates/core/src/metrics.rs`
  `VOICE_VOTE_KICK_OUTCOMES`). Recorded once per `kick_start` decision
  (`started` or the refusal code from `kick_refusal_outcome`) and once per
  terminal `KickMember` enforcement (`EnforcementOutcome::as_str`); vote
  results are audit-only. Refusal codes never become free-form text; unknown
  outcomes collapse to `other`.
- `two_bot_event_voice_tick_total{outcome}` — `outcome` is `opens_soon`,
  `rate_limited`, `perm_denied`, `no_category`, `unavailable` or `other`
  (`crates/core/src/metrics.rs` `EVENT_VOICE_TICK_OUTCOMES`). Recorded once
  per event-voice preflight that returns `Waiting`, including backoff-skipped
  ticks, with the website reason (`EventVoiceTbdReason::as_str`); `Ready`
  ticks record nothing here. The ticker card extends this same series with
  its ready/create family. Unknown outcomes collapse to `other`.
- `two_bot_voice_join_occupancy_total{occupancy}` — `occupancy` is `0`, `1`,
  `2`, `3`, `4`, `5`, `6` or `7+` (`crates/core/src/metrics.rs`
  `VOICE_JOIN_OCCUPANCY_BUCKETS`). Recorded once per organic ticketed join by
  `Metrics::voice_join_occupancy` from the gateway frame handler; leaves,
  same-channel updates, pre-READY frames and mechanical transitions
  (creator/generator target or previous channel) are not samples, while
  session-lobby and permanent protected-channel joins still sample. No member,
  channel or time labels and no per-event log line; hourly curves use
  `increase(...[1h])` (see `docs/voice-occupancy-curve.md`).
- `two_bot_db_errors_total{op}` — `op` is `admission` or `other`. Recorded
  by `Metrics::db_error`; send-admission SQL (admit/extend/complete storage
  failures) reports as `admission`, and failed voice actor store loads
  report as `other`.
- `two_bot_containment_store_errors_total{op}` — `op` is `complete_incident`
  or `other` (`CONTAINMENT_STORE_OPS`). Recorded by
  `Metrics::containment_store_error`; a failed containment completion counts
  once and marks the staff alert outcome-unknown, without retrying the write.
  Unknown operations collapse to `other`; no error text becomes a label.
- `two_bot_send_admissions_total{outcome}` — `outcome` is `admitted`,
  `blocked`, `storage_error` or `other`. Recorded once per `admit()`
  decision by the Postgres admission gate; failed `complete()`/`extend()`
  storage writes count only in `two_bot_db_errors_total`.
- `two_bot_gateway_prefix_trigger_refused_total{reason}` — `reason` is
  `verdict` or `other`. Recorded once on the verdict-refusal arm in
  `crates/discord/src/custom_commands.rs` for prefix candidates only: the arm
  still returns `Refused` for every contained create, but unmatched content,
  scope mismatches and the disabled fast path never increment it. Alert rule
  `prefix_trigger_verdict_refusals` tickets when the `verdict` series rises
  in 3 consecutive keepalive samples (first sample is baseline-only;
  `other` is normal refusal volume and never counts).
- `two_bot_dispatch_drops_total{lane}` — `lane` is one of `messages`,
  `interactions`, `registry`, `privileged`, `busy` or `reactions`, in the
  bot's `DISPATCH_LIMITS` order (`crates/core/src/metrics.rs`
  `DISPATCH_LANES`). Recorded on every `spawn_first` saturation refusal,
  including the busy-lane path, plus per-member fairness refusals on the
  `reactions` lane (the `dispatch_self_role_reaction` per-member cap, via the
  shared `record_drop` path); scope-shutdown refusals are not drops.
- `two_bot_gateway_checkpoint_failures_total{stage}` — `stage` is
  `pre_commit` or `commit` (`crates/core/src/metrics.rs`
  `CHECKPOINT_FAILURE_STAGES`). Recorded once per `apply_dispatch` Err arm
  entry in `crates/bot/src/gateway.rs`, plus once per failed checkpoint
  clear on the `ReceivedWork::Clear` path: `pre_commit` when the commit was
  skipped after a funnel/leveling/acknowledgement failure, `commit` when the
  durable store write itself failed (including a failed clear). Failure
  causes are never labels. Alert rule `gateway_checkpoint_failures` fires
  on any increase summed over every `stage` between consecutive keepalive
  samples (the first sample is baseline-only; the failing process lingers
  75 s so a scrape lands before the restart resets the counter, and the
  post-restart reset sample resolves); unlike bursty
  dispatch drops, a single checkpoint failure stops the worker, so there is
  no benign-singleton case.
- `two_bot_drop_poison_total{guard}` — `guard` is one of
  `command_dispatch_guard`, `schedule_run`, `user_slot`, `in_flight_guard`,
  `ticket_supervisor`, `response_accounting` or `voice_connection_guard`
  (`crates/core/src/metrics.rs` `DROP_POISON_GUARDS`). Recorded once per
  poisoned drop through `recover_drop_poison`, alongside one
  `drop guard recovered a poisoned lock` warn carrying the guard name; the
  drop still releases its state from the recovered guard. Panic payloads
  are never labels.
- `two_bot_internal_actions_total{family,outcome}` — `family` is one of
  `announcement`, `event`, `settings`, `moderation`, `membership` or
  `other`, mapped from the signed `action` verb in
  `crates/bot/src/internal_action_http.rs` (`announcement.post` →
  `announcement`, `event.*` → `event`, `settings.*` → `settings`,
  `moderation.*` → `moderation`, `role.assign`/`guild.add_member` →
  `membership`, everything else → `other`). `outcome` is `executed` or one
  of the bounded refusal classes (`auth_failure`, `unknown_key`,
  `clock_skew`, `nonce_replay`, `rate_limit`, `unknown_action`,
  `action_disabled`, `malformed_body`, `conflict`, `upstream`, `internal`).
  Recorded once per receiver request (`ReceiverState::reject` for refusals,
  `ReceiverState::terminal` plus the keyless-read and settings/moderation
  success envelopes for executions); no key id, token, body or request bytes
  ever become labels.
- `two_bot_moderation_enforcements_total{verb,outcome}` — `verb` is one of
  `ban`, `tempban`, `kick`, `timeout`, `warn`, `purge`, `slowmode`,
  `lockdown` or `unlock` (exactly `ModerationAction::command_name`, in legacy
  publish order; `crates/core/src/metrics.rs` `MODERATION_VERBS`); `outcome`
  is `success` (completed enforcement, including idempotent replays served
  without a second effect), `refused` (policy/validation/definitive
  rejections that prove no mutation happened), `discord_error` (uncertain
  Discord failures: timeout, unreachable, rate-limited), `storage_error`
  (persistence failures) or `other`. Recorded once per terminal member
  enforcement (slash `MemberRuntime`, website `ModerationExecutor` plus
  `InternalMemberExecutor`), including definitive preflight refusals (slash:
  unknown roles and non-member target; website: resolver `ActionNotAllowed`
  such as non-member target, actor not member, or actor forbidden, recorded
  by the website effect before the executor runs). Slash member authority
  refresh failures also count once as `refused`, before any mutation; actor
  cooldowns are not outcomes. Website resolver snapshot failures and other
  indeterminate reads before terminal execution are not outcomes.
  Slash member authority diagnostics use `warn!` with the fixed message
  `member authority unavailable`, `verb`, and a bounded `cause`: `discord_403`
  (confirmed 403 on guild/member reads), `transport` (timeout, unavailable,
  rate-limit, guard or other HTTP refusal on those reads), `malformed_roles`
  (invalid JSON, incomplete/malformed guild or member evidence, or unknown held
  roles), `unknown_bot` (any failed bot-identity lookup, including HTTP failures,
  or absent/non-bot bot membership), or `guild_mismatch` (interaction or fetched guild
  identity differs from the configured guild). No provider text or payload is
  logged. Only the first refusal per cause per 60 seconds per member runtime is
  warned: five fixed sampling slots, no member-keyed state. Suppression never
  changes the enforcement counter, denial audit, or generic member reply;
  causes are log fields, not new metric labels or enforcement outcomes.
  Recorded once per terminal channel enforcement (slash
  `ChannelModerationRuntime`, website `ChannelModerationExecutor` plus
  `InternalChannelExecutor`), with the same boundary: website resolver
  `ActionNotAllowed` records `refused` at the effect, snapshot failures stay
  unsampled. Retries and rate-limit backoffs are not outcomes. Channel
  slash enforcement records uncertain outcomes as `other`; website enforcement
  records uncertain Discord failures as `discord_error`. A purge timeout that
  proves no deletion was sent is recorded as `refused`. No user id, reason
  text or free-form detail ever becomes a label.
- `two_bot_community_facts_drain_failures_total{reason}` — `reason` is `error`,
  `timeout` or `other`. Recorded once per failed community-facts drain in
  `crates/bot/src/gateway.rs` (`apply_dispatch`), in both warn-and-continue
  arms; the existing warn log lines are unchanged. No error text, SQL or
  identifier becomes a label.
- `two_bot_feed_fetches_total{outcome}` — `outcome` is `success`,
  `invalid_source`, `refused_redirect` or `failure`
  (`crates/core/src/metrics.rs` `FEED_FETCH_OUTCOMES`). Recorded once per
  fetch in `crates/bot/src/feed_jobs.rs` via
  `crates/core/src/feeds_connector.rs` `record_feed_fetch`; per-outcome,
  never per-feed, so no source URL, host or feed id becomes a label.
- `two_bot_feed_fetch_errors_total{error_class}` — `error_class` is
  `invalid_source`, `non_public_address`, `redirect_refused`, `too_large`,
  `invalid_encoding`, `unsupported_content_type`, `unsupported_encoding`,
  `resolve`, `transport`, `deadline`, `http_error` or `other`
  (`crates/core/src/metrics.rs` `FEED_FETCH_ERROR_CLASSES`). Recorded once
  per failed fetch alongside the outcome above; successes record nothing
  here. No URL, host, header or body bytes become labels.
- `two_bot_ticket_lifecycle_total{event,outcome}` — `event` is one of
  `retire`, `abandon`, `purge`, `claim` or `close`
  (`crates/core/src/metrics.rs` `TICKET_LIFECYCLE_EVENTS`); `outcome` is one
  of `retired`, `abandoned`, `already_gone`, `deleted`, `claimed`, `closed`
  or a storage-failure `db_cause` (`unique_violation`,
  `foreign_key_violation`, `not_null_violation`, `check_violation`,
  `exclusion_violation`, `database`, `pool_timeout`, `pool_closed`,
  `worker_crashed`, `connection`, `row_not_found`, `query`)
  (`crates/core/src/metrics.rs` `TICKET_LIFECYCLE_OUTCOMES`, sharing
  `crates/core/src/error_chain.rs` `db_cause`). Recorded once per
  instrumented terminal ticket event in `crates/bot/src/ticket_runtime.rs`
  (retire, abandon, purge, successful staff-button claim/close; recovery
  completions excluded) alongside its `ticket_lifecycle` log line; the purge
  pass reports its deleted count, not one per pass. No member text, channel
  name, ticket id or error string becomes a label; the closed vocabulary has
  no `other` row.
- Log fields (coordinated with blocked structured-log work, which owns JSON
  formatting): `voice_event="voice_operation"` with `op`/`outcome`,
  `voice_event="voice_reconcile"` with plan counts,
  `voice_event="voice_dead_letter"` with `action`/`attempts`, and
  `voice_event="voice_creator_orphan"`. No channel, member, token, body or
  ID leaves the process in any label or field.
- Worker log fields (Workers Logs only, never a metric):
  `event="container_gateway_failure"` with `phase`/`class`/`failed_step`/
  `error_code`/`build_revision`/`deploy_id`/`release`/`worker_version`/`log_url`, emitted
  by the Container DO keepalive in `wrangler/src/index.ts` once per tick while
  the gateway task is failing. `phase` is `durable_gateway`; `class` is one of
  `store_unavailable`, `gateway_pool_connect_failed`, `checkpoint_load_failed`,
  `onboarding_gates_invalid`, `onboarding_init_failed`,
  `custom_commands_init_failed`, `milestones_load_failed`,
  `automod_config_invalid`, `automod_executor_failed`, `raid_executor_failed`,
  `gateway_runtime_failed` or `gateway_task_panicked` (every `FailureClass`
  variant in `crates/bot/src/gateway_failure.rs`). Only `phase` and `class`
  use `[a-z0-9_]{1,32}` tokens; anything else is dropped. `failed_step` names
  the fallible step, `error_code` carries the SQLSTATE (`none` when no database
  code applies). `release` is the container git SHA from `/readyz`
  `build_revision` (stamped at deploy time, `"unknown"` when unparseable, never
  echoed); `build_revision` carries the same SHA under its `/readyz` spelling
  and `log_url` links the log-query docs; `worker_version` is the platform
  Worker version id. `metrics_alert` lines carry the same `release` and
  `worker_version` from that tick's probe. Group gateway failures by `class`;
  group metrics alerts by the alert rule in `content`. Error-class grouping
  applies to metrics alerts only when `content` includes a validated last error
  class, as with `job_consecutive_failures`. See
  [startup diagnostics](startup-diagnostics.md#self-diagnosing-gateway-failures-readyz-gateway_failure).
- `two_bot_voice_join_to_move_seconds` is a label-free histogram from creator
  join gateway receipt to successful member move. Cancelled/failed joins do not
  add samples. Its buckets (`le`, seconds) are `0.001`, `0.005`, `0.01`, `0.05`,
  `0.1`, `0.5`, `1`, `5`, `+Inf`, plus `_sum` and `_count`.
  `voice_event="voice_join"` / `voice join completed` emits one terminal log for
  an accepted join, with bounded `outcome`, `queue_dwell_seconds`,
  `wait_before_dispatch_seconds`, `claim_seconds`, `reorder_seconds`,
  `post_seconds`, `persist_seconds`, `move_seconds` and `elapsed_seconds`.
  Stage durations accumulate across 429 retries. `reorder_seconds` is
  retained for log-schema compatibility and is always 0: packed-category
  respacing no longer runs before the create, it runs as a post-move repair
  after the join-to-move measurement (see the `voice_respace` log line).
  No guild/member/channel identifiers or user content are attached.
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

## Read-only ops roll-ups (`/ops/summary`, `/ops/gateway`, `/ops/backfill`)

Three JSON roll-ups on the same listener reuse the `/readyz`, gateway,
job, pool and event-store sources above. Like `/metrics` they are served
openly on the container port. The off-container path is the Worker, which
exact-matches the three paths before the invite-redirect lookup:
`/ops/gateway` and `/ops/backfill` stay gated by the scrape bearer
(`METRICS_SCRAPE_TOKEN`, 404 when unset or short, 401 on a wrong bearer,
429 per-caller attempt budget), while `/ops/summary` accepts either the
scrape bearer or the dashboard service-token pair
(`OPS_DASHBOARD_CLIENT_ID` / `OPS_DASHBOARD_CLIENT_SECRET`, same 404/401/429
shape). The scrape gate runs first and its 429 is a hard fence; a
non-throttled scrape refusal falls through to the dashboard gate.
Doubly-unconfigured still 404s, otherwise a failed attempt is 401 with the
Bearer challenge. The DO proxies the roll-ups with the same 6 s deadline and
64 KiB body cap as the scrape, fetching the container's `/ops/summary`
(never `/readyz`) and rewriting a 200 whose body has `ready: false` into the
503 the dashboard renders (the roll-up itself always answers 200). Closing
the summary requires removing both `METRICS_SCRAPE_TOKEN` and the dashboard
pair; removing only one leaves the other credential open.

`GET /ops/backfill/progress` (TOG-22953, per-family progress/ETA) is forwarded
exact-path too, but it is never scrape-gated: the container owns its `x-two-*`
HMAC (same ring as the internal-action receiver, path-bound). The Worker
enforces GET-only (405 otherwise), a per-caller attempt budget (429 with
retry-after, taken before the header check) and an `x-two-*`
presence/length pre-check against the receiver's caps (the route's
indistinguishable 401) — all before the singleton DO is touched — then stamps
the deployment fence id and forwards the request. The DO proxies it with the
`x-two-*` headers attached (a bare string-URL fetch would drop them and turn
every call into a 504) and keeps the container's 401 as a fixed 401 — other
non-2xx stay 504. Auth failures are the route's indistinguishable 401; see
`crates/bot/src/ops_backfill.rs` for the contract.

- `GET /ops/summary`: readiness (`ready`, `components`), gateway state and
  failure, the informational `jobs` map, pool gauges (`size`/`idle`/`max`),
  the rejected-duplicate-registration count
  (`duplicate_pool_registrations`, always present, `0` on unconfigured
  boots), event-store lag (`restorable_point`, `max_lag`: the largest
  per-table lag) and backfill progress (`tables_total`, `tables_at_point`:
  tables whose lag is exactly 0), plus the build revision/id.
- `GET /ops/gateway`: shard state (`unconfigured`/`armed`/`connected`/
  `draining`), gateway status, the last gateway-task failure, reconnect /
  resume / disconnect / missed-event counters, `last_ready_unix` and
  `last_resume_unix` (Unix seconds of the last observed READY/RESUMED
  dispatch), per-event counts, per-stage checkpoint failures, and the
  `session_checkpoint` job's last success.
- `GET /ops/backfill`: the global restorable point plus every covered
  journal table's durable cursor (`watermark`) and signed per-table `lag`:
  the table's own newest journal id minus its watermark, so 0 means fully
  journaled no matter how much other tables have written since. A table
  with no journal rows or no committed watermark reports `null`, never zero.

Every database-backed section degrades to an explicit `available: false`
with `200`, never a 500 or an error string: a database outage must not hide
gateway and job health. Event-store reads share the 2 s `/readyz` probe
budget and run as one statement over the covered tables (per-table index
seeks, one snapshot), so a commit cannot pair a new tip with an old
watermark. Responses carry `no-store`; bodies are `application/json`.

## Cardinality and memory

All retained metric values are fixed-size arrays. Only compile-time allowlisted
labels reach exposition. REST paths are matched to templates, never copied as
labels: no guild/member/channel IDs, tokens, query strings, visitor data or message
content. Unknown routes/events/jobs collapse to `other`. Histograms have eight
fixed finite buckets and `+Inf`, sum and count. A scrape allocates a bounded-size
text response; observations retain no input string. Wire `HELP` descriptions stay
concise to preserve the 64 KiB scrape budget as fixed series are added; the catalog
above retains the detailed semantics. Shortening `HELP` prose does not change
metric names, types, labels, values or series counts.

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
- Docs conformance (`crates/core/tests/metrics_docs.rs`): every counter row
  in the table above renders in the exposition with its allowlisted labels,
  and every exposition counter has a row here; the job error-class gauge is
  pinned to its documented labels and complete fixed `JOBS × ErrorClass`
  series. Deliberate-drift fixtures prove counter drift fails by name.
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

Every DO ops read — the background alert scrape, the summary proxy, the
read-only ops proxies (metrics, gateway, backfill and the signed
backfill-progress read) and the keepalive `/readyz` probe — shares the
streaming 64 KiB body cap and a single end-to-end six-second deadline across
the container fetch and the body read. The deadline does not rely on the SDK
or the response stream honoring abort: a fetch or body that ignores
cancellation still settles the caller at six seconds, a response that only
arrives after the deadline is released and discarded (never proxied, never
recorded as readiness), and body cancellation is best-effort — a cancel hook
that stalls or never resolves cannot extend the caller's critical section.
Non-OK bodies are cancelled without buffering; oversize or stalled bodies are
cancelled too, without waiting on a cancellation hook. Failed scrapes log
only `two-bot metrics scrape failed`: no response body,
exception detail, token or URL. Empty/non-exposition, malformed or interrupted
bodies, timeouts, and missing/duplicate/invalid alert inputs keep the previous
firing set, counter baselines and streaks, and emit no `RESOLVED` — except the
dispatch-drop baselines and streaks and the event-store lag streak, which reset
without resolving active tickets (a failed scrape invalidates all dispatch
baselines and breaks the lag streak).
The validator requires every always-emitted series consumed by the rules,
including all current label combinations from `crates/core/src/metrics.rs`,
with finite nonnegative integer inputs (pool-configured is 0 or 1). Additional
series are accepted, including new label values with the same consumed schema;
unrelated `NaN` latency and histogram `+Inf` remain valid Prometheus values.
A complete valid scrape still persists state before notifying. This validation
is only for background evaluation; the authenticated pull remains a bounded
exposition proxy, and ownership and bearer controls are unchanged.

- Authenticated pull: `GET /ops/metrics` on the Worker with
  `Authorization: Bearer <METRICS_SCRAPE_TOKEN>`. The token is an optional
  Worker secret (never a plain var) and must be at least 32 characters —
  the same floor as the ownership control token. Unset or shorter → `404`
  (plus one redacted log line naming the requirement); a short staging
  token must be reissued, never padded (none is provisioned today).
  Missing or wrong bearer → `401` (compared via SHA-256 digests);
  every attempt takes one token synchronously before the comparison, so
  concurrent guesses cannot share a token and a throttled caller is
  refused without any comparison — guessing cannot confirm a bearer
  while exhausted (`429` + `retry-after` via the existing per-caller
  bucket, 10 burst, 1/sec). Buckets are per caller, so another caller's
  guessing cannot throttle a correct bearer elsewhere; a caller shed only
  because the 10,000-entry table is full is still compared, so a scanner
  flood cannot lock out the authenticated scraper. The scraper
  (~1/15 s) never nears the budget. Non-GET →
  `404`. Unauthenticated requests never reach the container. `/metrics`
  itself stays `404`. The ownership control path (`/internal/ownership`)
  keeps its own gate and shares neither this bucket nor its budget.
  The DO container fetch and its body read share one end-to-end 6 s
  deadline (`504` at the deadline, generic body — the SDK resolves aborts
  as a 500 Response, which is mapped to 504 without proxying its text, and
  a fetch or stream that ignores abort still settles at the deadline) and
  the upstream body is capped at 64 KiB (larger → `502`, generic body).
- Rules live in `wrangler/src/alert-rules.ts`; each links to a
  [runbook](runbook.md#metrics-alerts) section (a test enforces the anchors):

| Rule | Fires when | Runbook |
| --- | --- | --- |
| `job_stale:<job>` | last success older than 2 x the job cadence (never-succeeded is ignored) | [job stale](runbook.md#alert-job-stale) |
| `job_consecutive_failures:<job>` | `two_bot_job_consecutive_failures` >= 3 | [job failures](runbook.md#alert-job-failures) |
| `rest_429_rate` | 429s > 10% of REST requests between samples, >= 10 requests | [REST 429](runbook.md#alert-rest-429) |
| `rest_5xx_rate` | 5xx responses > 10% of REST requests between samples, >= 10 requests | [REST 5xx](runbook.md#alert-rest-5xx) |
| `db_pool_saturated` | pool at max, 0 idle, 3 consecutive samples | [DB pool](runbook.md#alert-db-pool) |
| `db_errors` | 3+ storage failures between samples (restarts skip the window) | [DB errors](runbook.md#alert-db-errors) |
| `send_admission_blocked` | new admission refusals in 3 consecutive samples | [send admission blocked](runbook.md#alert-send-admission-blocked) |
| `voice_failures` | room-op failures > 5% of >= 10 ops between samples, any new dead-letter/orphan, or any new vote-kick enforcement failure (`discord_error`/`gave_up`/`permission_missing`; benign refusals and terminal skips never fire; missing/invalid samples hold, restarts skip the window) | [voice failures](runbook.md#alert-voice-failures) |
| `gateway_missed_events` | any increase of `two_bot_gateway_missed_events_total` between samples (first sample and restarts skip the window) | [gateway missed events](runbook.md#alert-gateway-missed-events) |
| `gateway_unpaired_disconnect` | `two_bot_gateway_disconnects_total` grew and RESUMED plus fresh READY (`two_bot_gateway_events_total{event="READY"}`) have not grown for 3 consecutive samples; any RESUMED or fresh READY growth clears at once (first sample sets the baseline; counter decreases or validated same-scrape process-identity changes re-baseline and hold a firing alert for one sample; legacy identity adoption never clears by itself; missing identity after attribution and invalid samples hold state) | [gateway unpaired disconnect](runbook.md#alert-gateway-unpaired-disconnect) |
| `gateway_not_ready` | `/readyz` gateway component non-ready (`starting`/`down`) for 3 consecutive valid samples with no disconnect edge required; any ready sample clears at once (first valid sample sets the baseline, restarts re-baseline and hold a firing alert, invalid probes/scrapes change nothing) | [gateway not ready](runbook.md#alert-gateway-not-ready) |
| `ticker_stale:<job>` | 15 s ticker with no success for more than 10 minutes (never-succeeded is ignored) | [ticker stale](runbook.md#alert-ticker-stale) |
| `receiver_refusals:<family>` | refused `two_bot_internal_actions_total` outcomes rising in 3 consecutive samples per family (first sample and restarts clear the streak) | [receiver refusals](runbook.md#alert-receiver-refusals) |
| `dispatch_drops:<lane>` | **ticket**, not page: `two_bot_dispatch_drops_total{lane}` grows in 3 consecutive completed sample windows per bounded lane; first sample is baseline, flat/reset clears, missing/invalid retains active alerts | [dispatch drops](runbook.md#alert-dispatch-drops) |
| `job_unknown_stale:<job>` | **ticket**, not page: job without a cadence entry with no success for more than 2 hours (never-succeeded is ignored) | [job unknown stale](runbook.md#alert-job-unknown-stale) |
| `gateway_checkpoint_failures` | any increase of `two_bot_gateway_checkpoint_failures_total` summed over every `stage` between samples (first sample is baseline-only; the 75 s runtime linger leaves ~3 s margin over a 60 s tick plus two 6 s probes for a pre-exit scrape, the post-restart reset resolves) | [checkpoint failures](runbook.md#alert-checkpoint-failures) |
| `voice_join_fast_burn` | creator-join budget (99.5% moved within 5 s) burning faster than 14.4x in both the 1 h and 5 min windows; each window requires at least 10 genuine joins and two bad moves; receive counts lag one scrape and incomplete post-restart windows stay inactive | [voice join fast burn](runbook.md#alert-voice-join-fast-burn) |
| `voice_join_slow_burn` | **ticket**, not page: same budget burning faster than 6x in both the 6 h and 30 min windows, with the same per-window volume and bad-move floors (sustained grind the fast rule never catches) | [voice join slow burn](runbook.md#alert-voice-join-slow-burn) |
| `voice_creator_map_stale` | `two_bot_voice_creator_map_age_seconds` > 600 (two missed 5 min refreshes) with `two_bot_voice_creator_map_generation` > 0; generation 0 (never merged) never fires, missing gauges on older containers hold the previous state | [voice creator-map stale](runbook.md#alert-voice-creator-map-stale) |
| `prefix_trigger_verdict_refusals` | **ticket**, not page: `two_bot_gateway_prefix_trigger_refused_total{reason="verdict"}` rises in 3 consecutive keepalive samples; first sample is baseline, flat/reset clears, missing/invalid retains active alerts | [prefix trigger verdict refusals](runbook.md#alert-prefix-trigger-verdict-refusals) |
| `community_facts_drain_failures` | **ticket**, not page: summed `two_bot_community_facts_drain_failures_total{reason}` growth in 3 consecutive completed windows; first sample is baseline, flat/reset clears, missing/invalid retains active tickets | [community facts drain failures](runbook.md#alert-community-facts-drain-failures) |
| `eventsub_post_failed_share` | **ticket**, not page: failed `two_bot_eventsub_posts_total` > 10% of attempted posts, >= 10 attempted (sub-floor windows accumulate across consecutive valid samples until the floor is met; first sample is baseline, resets rebaseline, missing/invalid preserves, and flat/sub-floor windows hold an active ticket until a judged window clears it) | [EventSub post failed share](runbook.md#alert-eventsub-post-failed-share) |
| `event_store_consumer_lag` | **ticket**, not page: `two_bot_event_store_consumer_lag_seconds` > 60 in 3 consecutive valid samples (fractional counts); a valid sample at or below 60 resolves and rebaselines, missing/invalid samples and failed scrapes break the streak but preserve a firing ticket | [event-store consumer lag](runbook.md#alert-event-store-consumer-lag) |
| `error_budget_fast_burn` | REST error budget (99.9% success over 48 h) burning faster than 12x in both the 15 min and 2 min windows; each window needs at least 10 eligible requests and two budget errors (`5xx` plus `transport`); incomplete post-restart windows stay inactive and missing inputs hold the previous state | [error budget fast burn](runbook.md#alert-error-budget-fast-burn) |
| `error_budget_slow_burn` | **ticket**, not page: same budget burning faster than 3x in both the 2 h and 15 min windows, with the same per-window volume and error floors; needs full 2 h history, holds on missing inputs, scrape gaps, or counter resets, and resolves on a covered quiet or below-threshold 15 min window | [error budget slow burn](runbook.md#alert-error-budget-slow-burn) |
| `rest_admission_breaker_open` | **ticket**, not page: `two_bot_rest_guard_breaker_open` = 1 for 3 consecutive samples (local refusals precede any REST wire attempt, so the 429/5xx rules cannot see them); one valid closed sample recovers, missing/invalid samples hold a firing ticket without manufacturing evidence | [REST admission breaker open](runbook.md#alert-rest-admission-breaker-open) |
| `rest_token_invalid` | bot-token latch `two_bot_rest_guard_token_invalid` = 1 on any valid sample (credential death; every REST send refuses and `/readyz` is 503 until a corrected restart); a valid 0 resolves, missing/invalid samples hold the page | [REST token invalid](runbook.md#alert-rest-token-invalid) |
| `rest_refusal_surge` | combined `two_bot_rest_guard_invalid_requests_total` + `two_bot_rest_guard_rejected_requests_total` growth of 10+ between samples (first sample is baseline-only, a backwards counter re-baselines only that family, below-threshold windows resolve, missing/invalid samples hold a firing page) | [REST refusal surge](runbook.md#alert-rest-refusal-surge) |
| `rest_global_responses_stuck` | `two_bot_rest_guard_pending_global_responses` nonzero for 3 consecutive valid samples (stuck bodies hold admission even at zero remaining); a valid zero resolves, missing/invalid samples and failed scrapes break the streak but preserve a firing page (the paused/remaining gauges stay rule-free by design: a live deadline is healthy backpressure and the countdown cannot wedge) | [REST global responses stuck](runbook.md#alert-rest-global-responses-stuck) |
| `audit_delivery_halt` | **ticket**, not page: label-free halt gauge `1` for 3 consecutive valid samples with no current `audit_retry` failure; valid `0` recovers once, missing/invalid data holds an active ticket | [audit delivery halt](runbook.md#alert-audit-delivery-halt) |
| `eventsub_tampered` | `two_bot_eventsub_classified_total{kind="tampered"}` rises in 3 consecutive samples (first sample is baseline-only, flat resolves, resets rebaseline, missing/invalid preserves firing alerts) | [EventSub tampered](runbook.md#alert-eventsub-tampered) |
| `eventsub_reconcile_failed` | **ticket**, not page: `two_bot_eventsub_reconcile_passes_total{outcome="failed"}` rises between samples (one ticket per failed pass however many error sites moved; first sample is baseline-only, a restart sample re-baselines and fires when the new process already counted a failed pass (failed startup pass) else holds a firing ticket, a healthy-pass increase with no failed increase resolves, flat/pending-only growth and missing/invalid samples hold) | [eventsub reconcile failed](runbook.md#alert-eventsub-reconcile-failed) |
| `voice_worker_halted` | `two_bot_voice_worker_halted` reads 1 (any guild actor halted); a valid 0 clears at once, missing/invalid holds | [voice worker halted](runbook.md#alert-voice-worker-halted) |
| `voice_live_self_heal` | **ticket**, not page: `two_bot_voice_live_self_heal_total` rises between valid samples (first sample is baseline-only, restarts re-baseline, a missing family is no window) | [voice live self-heal](runbook.md#alert-voice-live-self-heal) |
| `voice_live_not_ready` | `two_bot_voice_live_not_ready_seconds` past 120 s on a connected gateway (promotes the host-watchdog bound; the sibling keeps the age climbing through a hang); at or below clears, missing/invalid holds | [voice live not ready](runbook.md#alert-voice-live-not-ready) |
| `voice_queue_stall` | **ticket**, not page: `two_bot_voice_queue_oldest_age_seconds` past 120 s (matches the in-process stuck horizon; the sibling keeps the age climbing through a hang); at or below clears, missing/invalid holds | [voice queue stall](runbook.md#alert-voice-queue-stall) |
| `gateway_drain_skips` | **ticket**, not page: any increase of `two_bot_gateway_drain_skips_total` between valid samples; first sample and resets baseline, missing/invalid family retains the baseline without a window verdict. A flat sample resolves; forwarding/deployment is outside this slice | [gateway drain skips](runbook.md#alert-gateway-drain-skips) |
| `retention_replay_needed` | **ticket**, not page: any increase of `two_bot_retention_replay_needed_total` between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [retention replay needed](runbook.md#alert-retention-replay-needed) |
| `voice_dispatch_timeouts` | **ticket**, not page: any increase of `two_bot_voice_dispatch_timeouts_total` between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [voice dispatch timeouts](runbook.md#alert-voice-dispatch-timeouts) |
| `voice_watchdog_stucks` | **ticket**, not page: any increase of `two_bot_voice_watchdog_stucks_total` between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [voice watchdog stucks](runbook.md#alert-voice-watchdog-stucks) |
| `voice_halt_recoveries` | **ticket**, not page: any increase of `two_bot_voice_halt_recoveries_total` between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [voice halt recoveries](runbook.md#alert-voice-halt-recoveries) |
| `rest_breaker_opens` | **ticket**, not page: any increase of `two_bot_rest_guard_breaker_opens_total` between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [REST breaker opens](runbook.md#alert-rest-breaker-opens) |
| `rest_global_pauses` | **ticket**, not page: any increase of `two_bot_rest_guard_global_pauses_total` between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [REST global pauses](runbook.md#alert-rest-global-pauses) |
| `gateway_latency_dead` | no `two_bot_gateway_latency_seconds` heartbeat-ACK evidence (NaN or unchanged finite value) for 3 consecutive valid samples; any fresh reading clears at once (first sample is baseline, missing/malformed samples hold, failed scrapes re-baseline and hold a firing page) | [gateway latency dead](runbook.md#alert-gateway-latency-dead) |
| `gateway_reconnect_flapping` | `two_bot_gateway_reconnects_total` rises in 3 consecutive samples (first sample is baseline, flat/reset clears, missing/invalid retains active alerts) | [gateway reconnect flapping](runbook.md#alert-gateway-reconnect-flapping) |
| `handler_duration_saturated` | **ticket**, not page: `two_bot_handler_duration_seconds` p99 past 1 s (tail share above 1% with >= 10 completions and >= 2 slow) in 3 consecutive windows; first sample is baseline, judged-quiet/reset clears, missing/invalid retains active tickets | [handler duration saturated](runbook.md#alert-handler-duration-saturated) |
| `job_failure_share:<job>` | **ticket**, not page: `two_bot_job_runs_total` failures > 10% of recent runs per job, >= 10 runs with >= 2 failures (sub-floor windows accumulate across consecutive valid samples; first sample is baseline, resets rebaseline, missing/invalid preserves, and flat/sub-floor windows hold an active ticket until a judged window clears it) | [job failure share](runbook.md#alert-job-failure-share) |
| `feed_fetch_failures` | **ticket**, not page: summed `two_bot_feed_fetch_errors_total{error_class}` rises between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [feed fetch failures](runbook.md#alert-feed-fetch-failures) |
| `eventsub_receive_gap` | **ticket**, not page: `two_bot_eventsub_received_total` exceeds summed `two_bot_eventsub_classified_total{kind}` for 3 consecutive samples (a closed gap resolves, a joint reset resolves, missing/invalid samples break the streak but preserve a firing ticket) | [EventSub receive gap](runbook.md#alert-eventsub-receive-gap) |
| `eventsub_reconcile_prior_failed` | **ticket**, not page: `two_bot_eventsub_reconcile_errors_total{error_class="reconcile_prior_failed"}` rises between samples after a healthy or pending pass completes with no failed-pass growth in the window (mid-pass scrapes retain all baselines; first samples and any counter reset re-baseline silently; a missing or invalid finding or pass-outcome row is no window) | [EventSub reconcile prior failed](runbook.md#alert-eventsub-reconcile-prior-failed) |
| `automod_sanction_refused` | **ticket**, not page: `two_bot_automod_decisions_total{outcome="sanction_refused"}` rises between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [automod sanction refused](runbook.md#alert-automod-sanction-refused) |
| `moderation_enforcement_failures` | **ticket**, not page: `two_bot_moderation_enforcements_total` summed over every verb and `discord_error`/`storage_error` rises between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [moderation enforcement failures](runbook.md#alert-moderation-enforcement-failures) |
| `voice_rest_stuck` | **ticket**, not page: `two_bot_voice_rest_in_flight_age_seconds` above 60 s in 3 consecutive valid samples (a valid sample at or below 60 s resolves, missing/invalid samples break the streak but preserve a firing ticket) | [voice REST stuck](runbook.md#alert-voice-rest-stuck) |
| `voice_global_backoff_capped` | **ticket**, not page: `two_bot_voice_global_backoff_capped_total` rises between samples (first sample is baseline-only, restarts re-baseline, missing family is no window) | [voice global backoff capped](runbook.md#alert-voice-global-backoff-capped) |

The halt gauge is optional-when-present for older containers, but strictly
validated as label-free and boolean. Its streak is persisted with the existing
DO metrics-alert state. An unreadable halt retains the Rust gauge, so a nonzero
`audit_retry` consecutive-failure count vetoes both new tickets and recovery
until a successful sweep; other failed audit sweeps conservatively hold too.
A process's boot-default zero cannot recover a ticket until the audit job has
a nonzero last-success timestamp.
Missing/invalid readings or failed scrapes reset the pending streak without
resolving an active ticket. This classifies an existing transition as a ticket,
not a new external ticket-system integration or permission to unhalt delivery.

`dispatch_drops` covers `messages`, `interactions`, `registry`, `privileged`,
`busy` and `reactions` independently (the Worker allowlist is pinned to Rust).
All six use ticket severity in the firing message. Reaction drops include
per-member fairness refusals; none of these keys proves gateway packet loss.
One baseline plus three rising samples is required (~3 minutes at the default
60 s keepalive cadence), not three samples including startup. A valid counter
must have one series with only `lane` and a nonnegative safe integer. Missing,
duplicate, nonfinite, fractional, negative or unsafe values, extra labels and
failed HTTP/body scrapes invalidate the affected baseline and break the streak;
a failed scrape invalidates all dispatch baselines. An active ticket is retained
across those gaps and the first valid return sample; subsequent growth retains
it, while a valid flat or backwards window resolves it. Legacy DO state without
the new per-lane maps reads as unbaselined. A backwards counter in any observed
lane signals a process reset: all valid lanes re-baseline with zero streaks and
resolve, even if another lane's new count overtook its previous value. Missing
lanes still retain active tickets. Dispatch-drop rules do not consume the
process-incarnation family: a restart whose new count already exceeds the old
count cannot be distinguished from growth by this rule.

`job_stale` uses `JOB_INTERVAL_SECONDS`, which must equal each scheduled job's
Rust `*_INTERVAL_MS / 1000`. The 15 s tickers (`scheduled_messages`,
`settings`) use `ticker_stale` with the explicit `TICKER_STALE_SECONDS` (600)
window instead: at two intervals a 15 s cadence would flap on a single slow
scrape, and skipped busy deadlines are neither success nor failure, so
`job_stale` and `job_consecutive_failures` cannot see a wedged ticker.
`invite_snapshot`, `other`, `audit_retry`, `feeds`, `self_role_recovery`,
`member_unban_sweep` and `automod_expiry` have no cadence entry; a stale
nonzero success timestamp on any of them — or on any future label the scrape
accepts before the catalog maps it — raises the `job_unknown_stale` ticket
above (`UNKNOWN_JOB_STALE_SECONDS`, 7200) instead of staying silent. `other`
is the catch-all for unknown job names only, so it has no single cadence.
`session_checkpoint` is event-driven (one success per durable gateway commit,
so idle guilds go stale while healthy) and is excluded from the fallback via
`UNKNOWN_STALE_EXCLUDED` in `wrangler/src/alert-rules.ts`: it never tickets.
`audit_retry` keeps its parked/halt reason in the catalog; a halted sweep still
records success, so only a truly wedged sweep tickets — check
`two_bot_audit_delivery_halt` (`1` engaged, `0` cleared) to confirm a halted
sweep is intentional before treating flat audit deliveries as a wedged job.
`feeds` is covered by
the coarse fallback, not a precise cadence, because its cadence is
live-configurable (60–86400 s) with an inner polling gate and
successful gated ticks are not polling freshness; skipped gated ticks return
success and reset the streak. `self_role_recovery`
keeps failure-only alerting: twice its 30 s cadence is too close to the ~60 s
scrape interval for safe `job_stale` paging, and this label-isolation change
adds no ticker-staleness policy. `member_unban_sweep` keeps failure-only
alerting for the same scrape-proximity reason, compounded by its 180 s attempt
timeout: one attempt may outlive its 30 s cadence. `automod_expiry` keeps
failure-only alerting with the coarse 2-hour fallback: its 60 s tick is a
no-op success until the gateway builds the activation, and it only does real
expiry work when `TWO_AUTOMOD=1`. `self_role_recovery`, `member_unban_sweep`
and `automod_expiry` each trigger `job_consecutive_failures` independently at
three completed failures; `feeds` does the same subject to the skipped-tick caveat
above. A sweep with no completions is covered only by the coarse 2-hour
fallback, not a precise cadence.
`wrangler/test/alert-job-catalog.test.ts` checks website/community cadence
catalogs and direct feed/self-role/unban-sweep/automod-expiry job producers.
It fails when a registered producer is absent from `JOBS` or a `JOBS` label
has neither a matching cadence, ticker_stale coverage, nor a reasoned
unknown-stale entry. Only a cadence, ticker coverage, or event-driven exclusion
in `wrangler/src/alert-rules.ts` retires the fallback; a reasoned note alone does
not.

Packet identity (TOG-12100): rule ids above are the single shared spelling
used on both sides of the B2 soak evidence seam. The Rust canonical list is
`ALERT_RULE_IDS` in `crates/core/src/evidence.rs`; the Worker mirrors it in
`packetFilename` (`wrangler/src/alert-rules.ts`). Every evidence/alert packet
is named `evidence-{ruleId}-{window}.json` (soak-ledger packets stamp the
`soak_expected_committed` ledger identity), so the QA evidence table can
attribute packets when several rules fire in one window. Both sides pin every
spelling with tests; the payload shape is unchanged.

DB-error coverage: `two_bot_db_errors_total` counts send-admission SQL
failures (`op="admission"`) and voice actor load failures (`op="other"`);
other stores adopt `Metrics::db_error` incrementally, so this is not a
database-wide total. Correlate voice load failures with `voice actor load
failed; retrying with capped backoff` in container stdout (transient
failures retry inside the actor task; a credential refusal logs `voice
actor load failed; credential refused, not retrying` instead).
Every load failure increments the counter, but that warn is throttled
process-wide to once per 300 s. A slow trickle below the burst threshold
stays silent in `db_errors`; job-failure rules cover failed jobs, not every
storage path.
A forced job failure on staging (three failures) raises
`job_consecutive_failures:<job>` within about one keepalive tick. Counter resets
(process restart) skip the 429 window. Alert state is persisted in DO storage
before notifying, so delivery is at most once per transition.
