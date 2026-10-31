# Time-boxed channel lockdown: retain manual unlock

## Disposition

**No build; retain the existing authorized manual lockdown and `/unlock` paths.**
Do not add automatic restoration, a due-time notification, or a scheduler from
this decision. This document records a design outcome, not an enabled feature,
permission grant, runtime acceptance, or approval to change a guild's permissions.
No authenticated guild-host intent or measured benefit has been established.

A temporary incident restriction can outlive the incident if staff forget to
unlock. Automatically ending it could reduce that burden, but only if the bot
can distinguish its original restriction from later staff decisions. The present
contract does not establish that ownership. A deadline is not renewed consent.

## Current behavior and its limits

Source baseline: `e1868d97d3519cae3f2ed91726e83e98f77dff0d`.

- `crates/core/src/channel_moderation.rs:233-250` plans lockdown of five
  `@everyone` bits: send messages, send messages in threads, create public
  threads, create private threads, and add reactions. It does not change
  `VIEW_CHANNEL`; lifting lockdown does not promise every member full access.
- `crates/core/src/channel_moderation_store.rs:144-173` records the prior
  overwrite, reason, lock timestamp and recovery generation. Repeated locks
  can preserve that generation while the live send deny remains set. It is a
  recovery/cleanup fence, not a unique revision of staff intent or an expiry.
- `crates/core/src/channel_moderation.rs:269-299` requires a recorded seed and
  a fresh overwrite for `plan_unlock`. It refuses send-bit drift, restores
  recorded lockdown bits only while they remain in the locked shape, preserves
  unrelated live bits and changed thread/reaction bits, and deletes an
  originally absent overwrite only if the resulting masks are both zero.
- `crates/discord/src/executor.rs:2502-2562` sends complete overwrite masks or
  deletes the overwrite. These adapter operations accept no expected-state or
  version predicate. A fresh read and a later write are separate operations.

The existing [lockdown acceptance contract](channel-lockdown-acceptance.md)
already documents this external-edit limitation. Per-channel bot serialization
coordinates participating bot writers; it cannot serialize direct Discord
administrative edits. Staff must coordinate those edits with bot commands.
Manual unlock remains an authorized, freshly inspected action, not an atomic
compare-and-swap guarantee or a universal removal of access restrictions.

## Alternatives considered

| Alternative | Benefit | Decision |
|---|---|---|
| Keep manual lockdown/unlock | No new background authority, job, schema or retained data | Choose this now; staff still need to follow up |
| End a requested deadline in a private attention/notification state | Could make forgotten locks visible without restoring permissions | Not dispatched; still needs demonstrated value and its own bounded design |
| Automatically restore at an explicitly confirmed deadline | Could remove a remembered follow-up | Refused under the present ownership and authority contract |

A GET/PUT sequence cannot distinguish an unchanged bot deny from a staff member
reasserting the identical deny. It also cannot prevent a staff change between
those requests from being overwritten. A database intent revision is necessary
for stale bot jobs, but does not solve either external-effect problem. A fresh
permission check before sending likewise does not establish atomic authority
revocation at the later Discord effect.

## Renewal and failure obligations for any reconsideration

The following are requirements for a possible future proposal, **not implemented
states, accepted capabilities, or permission to create implementation work**.
They apply only after the external ownership and current-authority gaps have an
independently accepted solution.

| Event | Required safe result |
|---|---|
| Repeated or renewed lockdown, including an identical-value lock | Invalidate the old deadline with a new durable intent revision; recovery generation alone is insufficient |
| Cancel, reschedule, or manual early unlock | Serialize with bot writers and compare the exact intent revision; cancellation must not claim success after an effect has entered an uncertain send |
| Unlock then relock, or delete then recreate a recovery row | A new intent identity prevents an old timer or cleanup from matching |
| Simultaneous confirmations, stale components or duplicate jobs | One revision winner; stale work makes no permission write and never rearms itself |
| Direct staff edit, including identical-value re-denial | Do not automatically restore without provable ownership at the external effect boundary |
| Drift, missing recovery seed, unreadable overwrite or unavailable fresh authority | Refuse automatic restoration; preserve recovery evidence and give a private, truthful inspection/recovery action |
| Scheduling moderator leaves or loses authority; bot authority is withdrawn | No implicit delegation to the bot or substitution of another moderator |
| Foreign guild, feature disabled, security hold or drain | No effect merely because the deadline elapsed; preserve all existing fences |
| Restart, missed deadline or clock discontinuity | No blind catch-up or early restoration; a persisted UTC deadline alone proves neither current intent nor authority |
| Channel missing or a 403/404 response | No permission write or guessed replacement channel; missing access is not affirmative proof of deletion |
| Timeout, cancellation, crash or ambiguous Discord outcome | Keep the durable effect/lane fence and recovery evidence; do not resend automatically |
| Confirmed effect with failed database settlement | Retry only safe settlement for that exact effect, not the permission mutation |

## Reconsideration and delivery boundaries

Reconsider only on materially new evidence: authenticated guild-host intent
bound to an exact proposal, demonstrated user value, independently accepted
external-effect ownership and current-authority proof, and a complete bounded
lifecycle/recovery contract. A future proposal must cover private preview and
confirmation, bounded duration and explicit absolute expiry, inspect/cancel/
reschedule UX, all-writer intent fencing, resource limits, retention/erasure,
rollback, and independent acceptance. Slowmode expiry, server-wide or recurring
schedules and raid automation are outside this proposal.

Deterministic models can test revision races and permission-bit preservation;
they cannot prove real Discord conditional-write behavior or current authority.
Any admitted implementation would need independent review and synthetic REST
plus authorized disposable-database evidence. No live moderation experiment,
staging/production database test, migration or credential action follows from
this document. Publication, a passing test, elapsed time or silence does not
activate a feature.

The selected outcome adds no runtime job, polling, schema, retention store,
service or spend. It needs no runtime rollback. A later decision may supersede
this record when the evidence changes; it must not reinterpret this no-build
outcome as positive intent or security acceptance.
