# Automation actor admission

Custom text triggers and LFG signup changes have separate, process-local
five-second cooldowns keyed by **guild and actor**, not by command, channel,
LFG post or selected role. One actor cannot bypass a window by choosing another
trigger or signup. Other actors and guilds retain their own windows; neither
map is shared with ordinary slash commands or moderation.

Five seconds permits deliberate role corrections while limiting a single
actor's distinct automated sends or signup refreshes to one admission per
window. This is an abuse bound, not a Discord REST rate-limit replacement.
Existing send pacing, durable event fences and bounded interaction queues still
apply. A refused event does not extend the deadline or append an audit row.
Text-trigger refusals are silent, without posting a message in the channel.
LFG signup refusals use the existing ephemeral interaction reply, without
changing participants or refreshing the channel post. Leaving and closing an
LFG post remain available during the signup window.

## Bounds and retries

Each surface tracks at most 16 guilds and 1,024 live actors per guild. A full
guild refuses new actors without evicting live windows or consuming another
guild's actor budget. A full guild map refuses new guilds. Entries expire after
five seconds and are pruned on the next admission attempt, including refused
attempts. There is no background task or unbounded event history in these maps.

A redelivery of the most recently admitted event may pass the local window
without extending it, so transient failures can still reach existing retry
logic. This permission is **not** an effect/replay claim. The durable service
fences decide whether it is safe to continue. Text redelivery cannot POST again.
An LFG signup retry that reaches the service reuses its deterministic outcome
audit without changing participants or appending another audit; it may retry
refreshing the existing channel message. Routed redelivery first needs a valid
Discord acknowledgement. An already-acknowledged interaction is rejected with
[error 40060](https://docs.discord.com/developers/topics/opcodes-and-status-codes#json-json-error-codes)
before the service runs, so gateway redelivery does not guarantee refresh
recovery. Refused distinct events never reach the durable fences.

## Restart contract

Restarting the process (or rebuilding a runtime) clears these local windows.
It can admit a new event immediately; the cooldown is not a durable quota across
restarts or multiple runtime instances. The bot runs one runtime per surface in
one container. Permanent `command.text_attempt` replay exclusion remains
unchanged, including after a restart or expiry of the local window. LFG's
signup outcome audit deduplicates admitted interaction retries while that audit
is retained. Mutation and audit are not atomic: a failure between them retains
existing store retry semantics, rather than a new exactly-once guarantee.

Timing uses Tokio's monotonic clock so fake-time fixtures exercise the exact
window boundary without wall-clock sleeps. See the
[Tokio Instant reference](https://docs.rs/tokio/1.53.1/tokio/time/struct.Instant.html).
No production or staging writes are used to test this admission behavior.
