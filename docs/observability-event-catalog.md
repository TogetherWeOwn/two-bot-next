# Observability event catalog (gateway/session path)

Read-only extract of the structured log event names and metric labels
emitted on the gateway cutover path. This file renames nothing and changes
no emit site; it records the current spellings so watch queries, scrapers
and alert rules can match them exactly.

Method: offline code search over `crates/bot/src/gateway.rs`,
`crates/bot/src/main.rs`, `crates/bot/src/server.rs`,
`crates/bot/src/shutdown.rs`, `crates/bot/src/gateway_metrics.rs`,
`crates/bot/src/gateway_failure.rs`, `crates/core/src/metrics.rs` and
`crates/bot/src/voice_rooms.rs`. File and line numbers were read on
2026-10-03; they drift as the code moves, so re-run the search before
quoting a line in an incident.

## Gateway lifecycle log lines

The message string (last argument to the `tracing` macro) is the event
name. Fields such as `sequence`, `resume`, `error`, `job_id` or
`guild_id` are context, not the name.

| Event name | Callsite | Meaning |
| --- | --- | --- |
| `durable gateway initialized; shard connecting` | `crates/bot/src/main.rs:516` | Supervisor built the shard; `resume=true` means a saved checkpoint was offered, `false` means fresh IDENTIFY |
| `gateway shard loop started` | `crates/bot/src/gateway.rs:495` | Reception task entered the Twilight stream loop |
| `gateway reconnect failed; Twilight will retry` | `crates/bot/src/gateway.rs:371` | Transport reconnect attempt failed; Twilight owns the retry |
| `cold resume committed; requesting voice snapshot via identify` | `crates/bot/src/gateway.rs:362` | Cold voice RESUME committed; reception IDENTIFies for a fresh voice snapshot |
| `gateway ready; checkpoint committed` | `crates/bot/src/gateway.rs:671` | Dispatch committed and shard marked Connected |
| `gateway leveling dispatch failed` | `crates/bot/src/gateway.rs:609` | Leveling funnel drain failed inside the checkpoint deadline |
| `gateway onboarding job invalid` | `crates/bot/src/gateway.rs:1066` | Dispatch-worker durable onboarding payload failed to serialize; recorded as a typed error, checkpoint unchanged |
| `onboarding interaction interrupted; member must reselect` | `crates/bot/src/gateway.rs:791` | Durable onboarding job recovered without callback credentials; kept as interruption receipt |
| `invite counter read unavailable; retaining snapshot` | `crates/bot/src/gateway.rs:899` | REST invite read failed; persisted baseline kept instead of an empty listing |
| `interaction acknowledgement blocked; advancing past lost callback` | `crates/bot/src/gateway.rs:287` | Ordered RSVP acknowledgement hit send-admission Blocked; checkpoint advances past the lost callback without replaying uncertain effects |
| `interaction response failed; not replaying command` | `crates/bot/src/gateway.rs:290`, `:294`, `:300` | Ordered RSVP preparation or completion failed without admission blockage; command advances without replaying uncertain effects |
| `READY identity differs from boot token; ordered identity not armed` | `crates/bot/src/gateway.rs:682` | READY application id differs from the boot/REST pin; ordered identity stays disarmed and its fence keeps refusing |
| `gateway prerequisites missing; gateway parked, /readyz reports down` | `crates/bot/src/main.rs:564` | Token, database URL or guild ID missing; shard never starts |
| `feature gates invalid; ordered interaction surface parked` | `crates/bot/src/main.rs:639` | Feature-gate parsing failed; gateway still boots, only the ordered interaction surface stays off |
| `moderation gates invalid; ordered interaction surface parked` | `crates/bot/src/main.rs:646` | Moderation-gate parsing failed; gateway still boots, only the ordered interaction surface stays off |
| `durable gateway failed; checkpoint unchanged, readiness unavailable` | `crates/bot/src/main.rs:617` | Gateway task failed with the fixed class in `error_class`; checkpoint not advanced |
| `gateway task stopped; container restart required` | `crates/bot/src/main.rs:709` | Supervisor saw the essential task end; process must restart from checkpoint |
| `gateway drain failed; restart required` | `crates/bot/src/main.rs:716` | Drain path failed; restart required |
| `container service failed` | `crates/bot/src/main.rs:586` | Service supervisor failed (`startup_phase=service_supervisor`) |
| `shutdown_deadline_exceeded: abandoning in-flight work` | `crates/bot/src/main.rs:724` | Drain deadline elapsed; in-flight work abandoned |
| `listening` | `crates/bot/src/server.rs:176` | HTTP listener bound; one line per process start |
| `SIGTERM received; draining` | `crates/bot/src/server.rs:221` | SIGTERM observed; effects drain before exit |
| `SIGINT received; draining` | `crates/bot/src/server.rs:222` | SIGINT observed; effects drain before exit |
| `shutdown_completed` | `crates/bot/src/server.rs:254` | HTTP graceful drain completed; stable `msg` name with no human message |
| `response failed` | `crates/bot/src/server.rs:85,93` | HTTP 5xx response from the trace layer; DEBUG for the routine `/readyz` 503, ERROR otherwise |
| `second shutdown signal received; exiting immediately` | `crates/bot/src/shutdown.rs:62` | Second signal during drain; process exits at once |
| `invalid shutdown timeout; using default` | `crates/bot/src/shutdown.rs:36` | `SHUTDOWN_TIMEOUT_SECONDS` unparsable; default deadline kept |
| `periodic job failed` | `crates/bot/src/jobs.rs:127` | Completed scheduled-job attempt failed; `job` and `error_class` name the job |
| `TWO_VOICE=1 but no discord token; voice rooms disabled` | `crates/bot/src/gateway.rs:968` | Voice gate on but no token; gateway continues voice-off |
| `TWO_VOICE=1 but no database URL; voice rooms disabled` | `crates/bot/src/gateway.rs:980` | Voice gate on but no database; gateway continues voice-off |
| `voice database unavailable; voice rooms disabled` | `crates/bot/src/gateway.rs:989` | Voice store connect failed; gateway continues voice-off |
| `voice rooms enabled; gateway sink attached` | `crates/bot/src/gateway.rs:995` | Voice runtime built; sink attached to the gateway writer |
| `voice HTTP setup failed; voice rooms disabled` | `crates/bot/src/gateway.rs:999` | Voice HTTP setup failed; gateway continues voice-off |

## Gateway metric event labels

Fixed allowlist in `crates/core/src/metrics.rs:11-30`, observed in
`crates/bot/src/gateway_metrics.rs:27-42`. Unknown dispatch types
collapse to `other`; scrapers must match these exact spellings.

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
| `VOICE_STATE_UPDATE` | `two_bot_gateway_events_total` | Voice state changed dispatch |
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

## Gateway failure classes (`/readyz` `gateway_failure`)

Enum in `crates/bot/src/gateway_failure.rs:80-95`, served as
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

| Field value | Callsite | Meaning |
| --- | --- | --- |
| `voice_event="voice_operation"` | `crates/bot/src/voice_rooms.rs:375` (`voice_operation succeeded`), `:382` (`voice_operation failed`) | One finished room create/move/delete outcome with bounded `op`/`outcome` |
| `voice_event="voice_reconcile"` | `crates/bot/src/voice_rooms.rs:1470` (`voice_reconcile planned`) | One reconcile pass plan size with per-action counts |
| `voice_event="voice_dead_letter"` | `crates/bot/src/voice_rooms.rs:2002` (`voice action dead-lettered`) | Queue write exhausted retries with bounded `action` and attempts |
| `voice_event="voice_creator_orphan"` | `crates/bot/src/voice_rooms.rs:4904` (`voice creator orphan needs manual deletion`) | Untracked creator-channel orphan needing manual deletion |

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
