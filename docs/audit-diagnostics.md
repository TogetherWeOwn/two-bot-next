# Private Discord operational diagnostics

The existing [operational mirror](audit-service.md) is the only delivery sink.
These diagnostics do not implement a public audit feed, change presentation,
create a channel, grant permissions, request intents or change retention.
Channel visibility and the configured audit/voice/moderation destinations remain
operator-owned. Delivery still refuses a public or foreign-guild destination.

## Actual capture coverage

| Input | Private record | Evidence and limits |
|---|---|---|
| Voice state channel boundary | join, leave, move; member/channel IDs | Pre-update cache versus received state; mute/deafen/video-only frames are not boundaries. No executor inferred from the subject. Cold/incomplete cache can miss a prior channel. |
| Member update | added/removed role IDs, nickname-changed flag | No nickname text. No baseline produces an explicit `baseline_missing` observation, not fictitious role grants. |
| Member add/remove | member ID, join/leave | Leaving does not prove kick, ban or voluntary departure. |
| Channel create/update/delete | channel ID; bounded old/new names; config snapshot; overwrite/topic-changed flags | Missing cached baseline is explicit. Topic content and overwrite member lists are not copied. Cached voice-state count is observational, not authoritative occupancy. |
| Role create/update/delete | role ID, permission bitfields, position/name-changed flag | No role name text. Old permissions are null without a baseline. |
| Guild update | guild/owner IDs, verification/MFA/filter levels, system flags/name-changed flag | No guild name, description, vanity URL or other free-form text. |
| Message update/delete/bulk delete | message/channel IDs or bulk count | No content, attachments, URLs, embeds, audio or DMs. |
| Application command interaction | interaction and command IDs; invoker; received phase | No command options, command text, component IDs, interaction tokens or user display names. A received command is not proof of execution. |
| Shared command-runtime defer/answer/finish | reply phase and bounded accepted/error class | Delivery/dispatch diagnostics only, not business-success assertions. Other responder implementations and prefix-command decisions are not covered by these reply hooks. |
| Voice-room rename worker | decision, attempt and result | Old/requested channel names, reason class, human count, readiness/generation evidence, UTC and per-attempt correlation. Rate limits, deferred/unknown outcomes and refused requests remain distinct. Detached completion records its outcome even if the actor's result channel closes. |
| Discord audit-log event | existing 14 moderation types plus guild update, channel create/delete, bot add, role create/update/delete | Uses the recorded executor when present. Channel-name diffs only for channel actions; free-form reasons are not retained. No audit polling or extra permission request. |

Audit-log events require Discord to deliver them under the existing intent and
`VIEW_AUDIT_LOG` grant. No delivery is invented when access is absent. Unsupported
families include threads/stages/invites/webhooks/integrations, message-create
content, and uninstrumented feed/scheduled/internal-action decision paths. A
bot action may have a separate Discord audit entry where Discord supports one;
that is not comprehensive internal decision coverage.

## Attribution, time and correlation

Gateway observations use `actorConfidence=unknown`; the affected member is not
an executor. Interaction actors and audit-log executors use `verified` only when
present. `actorType` is bot/user only with identity evidence, otherwise unknown.
No probabilistic actor inference is used, so there is no implicit probable tier.
An audit entry arriving after a change remains a separate record: proximity,
matching names or occupancy do not join it to a gateway frame.

Gateway `occurred_at` is the received UTC instant (`clock=gateway_receipt_utc`),
not a claimed Discord event timestamp. Audit entries use their snowflake's UTC
instant. A rename result records the result receipt time; its old/new and
occupancy fields describe the decision snapshot, not a fresh result-time read.
An accepted REST response is not a subsequent Gateway observation of the name.

Production gateway identities use a digest of the resumable session ID plus
sequence and event kind, never the raw session credential. Replayed sequence
numbers in the same session reuse that identity even when receipt time changes;
a new session or rapid later change has a distinct identity. Audit-log IDs and
interaction IDs keep their intrinsic dedupe keys. Each reply/rename attempt has
its own correlation identity so failures and retries are not coalesced away.

## Bounds, gaps and restart behavior

Producers enqueue without database/REST I/O or per-event spawned tasks. The one
process-local queue holds at most **256 rows**, each at most **16 KiB** serialized
(about 4 MiB payload maximum). Overflow/oversize drops the incoming row, preserving
older queued evidence. `audit_capture_gap` warns at power-of-two loss counts;
`audit_capture_swept` reports recorded/depth/dropped totals. No raw row or error
text is traced. `audit_capture_started` explicitly identifies volatile retention.

The existing 30-second `audit_retry` job inserts at most **25 captured rows** per
sweep with a **5-second total insert deadline**, then uses the existing bounded
25-row delivery sweep and its shared absolute deadline. This is not a high-volume
lossless archive: sustained capture above 25 rows per 30 seconds can overflow.
A failed/uncertain insert keeps the front row for the next sweep. The durable
entry-ID constraint makes acknowledgement-loss replay idempotent. The halt stops
delivery, not capture/recording; a failed capture flush reports a database job
failure and retains the queue.

**Admission is not a durable receipt.** A process stop before insertion can lose
queued rows even after the gateway checkpoint commits. Restart starts an empty
capture queue and resets its process-local drop counter; no exact restart-loss
count is claimed. Once inserted, the existing durable nonce/claim/history-marker
protocol owns retries and restart reconciliation. An uncertain POST is never
blindly resent. Retained delivery quarantine and halt signals remain unchanged.
This bounded nonblocking trade-off must not be presented as complete incident
history. Missing permissions, cold cache, capture overflow, restart, store outage
or delayed/unsupported Discord events leave observable or documented gaps.

Mirror messages are not captured on create; edits/deletes and channel changes
whose source is a configured sink remain store-only through the existing
self-channel guard, so diagnostic delivery cannot recursively post itself.

## Privacy and verification

Only channel names are intentionally retained as text, capped at 100 UTF-16
units, with mention neutralization, control-character removal and inline-code
escaping. They appear only in the existing private audit store/mirror, not process
logs. Channel names can themselves be member-derived; existing access, retention,
backup and erasure policy still apply. No new public projection is authorized.

Deterministic fixtures live in `audit_capture.rs`, `audit_changes.rs`,
`audit_rename.rs`, `audit_gateway_tests.rs`, `audit_runtime_tests.rs` and
`command_runtime_tests.rs`. They cover bounds/loss, replay identity, privacy,
explicit attribution, renamed-channel evidence, retry taxonomy, and disposable-DB
capture batch/ack-loss/new-kind round trips. Existing mirror/service suites cover
private-guild fencing, self-channel suppression, 429s, ambiguous sends and
restart reconciliation. GitHub Actions on the exact PR head is the CI of record;
fixture authorship alone is not a passing test or live acceptance receipt.

Deployment acceptance separately requires an authorized read of the actual
private destination's visibility and a bounded sample of existing bot posts,
least-privilege Security review, independent QA, and exact-head release approval.
Do not force member joins/moves/renames to manufacture evidence. For a real rename
chain, preserve channel identity, event versus receipt time, every observed
old/new value, occupancy provenance, bot intent and actor confidence. Readiness
alone does not prove any of those facts.
