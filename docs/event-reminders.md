# Event help: attendee reminders

Going and Interested RSVPs receive one reminder in the scheduled event's channel
on the first available scheduler tick in the final **15 minutes** before its
start. Declined RSVPs are not included. Cancelled, completed, already-active,
past, channel-less and attendee-less events in the event mirror are skipped.
The lead time is fixed; there is no configuration or new RSVP command option.

Reminders are **channel-only, never DMs**. Attendees are shown as `<@id>` mentions,
but the closed outbound mention policy suppresses notifications. Event-controlled
mass mentions are neutralized. No attendee list, event name or message content is
written to reminder logs or audit records.

When the complete attendee list cannot fit in one 2,000-UTF-16-unit message, or
the roster exceeds the RSVP bound, the channel still gets exactly one bounded
fallback notice instead of silence: the event name, the attendee count, the
start time and the event link, with no mentions and never a partial roster.
The count covers all Going and Interested RSVPs, even when the stored roster
exceeds the 1,001-member read cap; declined RSVPs do not count. The scheduler
revalidates that total alongside the capped member list before admission. The
fallback counts as the event's one delivery attempt.

The existing activation-fenced automation scheduler must be enabled; its normal
15-second cadence also handles these reminders, at most two events in the
ordinary pass. After a healthy tick, one immediate **urgent follow-up** may
attempt at most two more events whose starts fall within the next 15 seconds.
This avoids stranding a third event that first receives an eligible RSVP just
before its start. Nonurgent events wait for the next ordinary tick. The
follow-up shares the same activation fence, supervisor ownership, cancellation
and overall deadline; it never reruns ordinary scheduled messages, resets the
timeout or admits an event after its start. There is no unbounded drain: at
most four reminder attempts run in one supervised invocation. Larger urgent
bursts, slow delivery or exhausted deadlines remain best effort.
Ordinary scheduled messages run first as a separate phase: a completed
schedule-store failure that stops their batch still permits the bounded
reminder attempt on the same tick when activation and supervisor ownership
remain valid, sharing one overall deadline and cancellation. Either phase
failing reports the shared job as unhealthy (both store failures map to the
existing database class); prior completions in either phase remain committed.
Sustained scheduler/store trouble can therefore cause a reminder window to be
missed.
It reads the guild-scoped scheduled-event mirror and stored RSVPs, not a new live
Discord lookup. A mirror update after dispatch admission cannot recall a message
already in flight. As with other mirrored event views, a Discord cancellation
not yet observed by the poller is not known locally.

## Failure and restart behavior

This is a best-effort reminder, not a delivery guarantee. Each guild/event gets
**at most one automatic attempt**, even across restarts, late RSVPs, rescheduling,
or a lost success reply. Database trouble before admission sends nothing. A
refusal, rate limit, uncertain result or crash after admission can leave no
reminder; the next tick does not repost. An event whose attendees cannot all fit
in one 2,000-UTF-16-unit message gets the bounded fallback notice described
above, never a partial roster or a burst of messages. Events with invalid ids
still fail closed with no post.

The immutable `rsvp.reminder_attempt` fact in `automation_audit_log` is the
pre-wire uncertainty fence, following the existing scheduler and text-command
single-attempt delivery pattern. Its deterministic id is scoped to guild and
event, its actor is NULL, and its target is only the event id. The attempt
records `unknown` / `delivery_pending`; a separate `rsvp.reminder` fact records
`posted`, `failed` or `delivery_uncertain` when known. An attempt without a result
is unknown, not proof of non-delivery. Never delete these replay facts to retry.
The existing backup allowlist includes this table; member erasure leaves these
system-actor facts intact. New admission stops at 10,000 retained attempt facts
per guild rather than recycling replay evidence.

Do not reset fences based on a missing channel-history result or Discord's short
nonce-deduplication window. Inspect the event channel and audit facts instead.
Rolling back the code stops reminder scans; preserve the audit rows. No schema,
permission, command publication or production-enable changes accompany this slice.
