# Member data and operator erasure

This is an engineering inventory and runbook, not a new legal retention policy.
It describes the **migrated Next schema**, not legacy tables that do not exist here.
Discord-side message deletion, automatic retention timers and production execution
are outside this implementation's scope.

## What is stored

- Discord guild/member IDs, join/leave/activity milestones, invitation attribution
  and event metadata (`members`, `events`, `invite_snapshots`).
- XP totals, awards and cooldowns, seasonal projections, rank projections and raid exclusions
  (`member_levels`, `member_season_xp`, `xp_awards`, `xp_cooldowns`, `member_ranks`, `member_exclusions`).
- Join-risk evidence copied from legacy anti-raid scoring: member ID, account
  age, join time, score and reasons (`join_risk_flags`). Erased by member ID; the
  legacy TEXT `event_id` and `reasons_json` are also checked for the snowflake.
- Anti-nuke containment evidence: the executor and target IDs of observed audit
  events and the incidents they triggered (`containment_events`,
  `containment_incidents`). Erased with the member (as executor, as target, or
  when a serialized result names them); incidents go first because they
  reference their trigger event. An in-flight (`containing`) or `uncertain`
  incident is a replay/block guard and makes erasure refuse until it settles.
- Tickets: `tickets` and `ticket_transcripts` (opener or claimer; 90-day transcript purge applies independently).
- Temporary voice rooms: each tracked room stores its current owner and original
  creator IDs (`voice_rooms`), erased by either. Each accepted room create also
  leaves a reservation row with the requesting member's ID and a timestamp
  (`voice_create_reservations`, the rolling burst and cooldown history), erased
  with that member. Erasure removes the tracking rows
  only, never the Discord channel. Creator-channel configuration
  (`voice_creators`, including V9 text-channel name/viewer-role settings) and
  companion text-channel records (`voice_text_companions`, Discord channel IDs
  plus the creation-time settings snapshot) store no member IDs and are
  untouched by member erasure. A room's private flag and Join channel ID
  (`voice_rooms`) hold no member IDs. Its block list (`voice_room_blocks`) does:
  erasure removes the erased member's rows from every room's list, and a room
  erased with its owner takes its whole list with it (the rows cascade). Its approved-grant
  witness (`voice_join_grants`) does the same: erasure removes the erased member's rows from
  every room's grant list, and a room erased with its owner takes its grants with it
  (the rows cascade). Its owner-grant journal (`voice_owner_grants`) records
  prospective recipients, pending or completed: erasure removes the erased
  member's rows by (`guild_id`, `member_id`), and deleting a parent room
  removes its journal rows through the migration's `ON DELETE CASCADE`.
  Event voice presence spans (`event_voice_presence`) record per-member
  join/leave spans: erasure removes the erased member's rows, guild-scoped
  through the scheduled-event mirror. The channel claim row
  (`event_voice_channels`) carries no member identity and is untouched.
  Erasure removes database tracking only, never the Discord channel.
  Guild room-command controls
  (`voice_access_controls`: creation switch, required role ID, per-command role
  IDs) and room logging settings (`voice_logging_settings`: detail level, notice
  channel ID, mention role ID) hold channel and role IDs only and no member IDs.
  The room vote-kick audit trail (`voice_vote_kick_audit`: one row each for a
  vote's start or refusal, its result and its enforcement outcome, with fixed
  outcome codes and no names, text or interaction tokens) stores the initiator
  and target member IDs, so erasure removes every row where the member is
  either one. It records a room-scoped disconnect, never a guild kick.
  The event-store voice shape (`event_store`) is provisioned by the S4 migration
  for a future gateway writer (guild / member / channel / session IDs plus a
  nullable `message_id` for S1 message-row coexistence, timestamps and enum
  mute/deaf/video/stream flags, no audio, transcripts, tokens or endpoints);
  erasure already removes every row for the member.
- Thread lifecycle and membership history (`event_store`: one row per
  THREAD_CREATE / UPDATE / DELETE, one row per listed thread per
  THREAD_LIST_SYNC fan-out, one row per THREAD_MEMBER_UPDATE / REMOVE delta).
  Rows hold guild, thread and parent IDs, the archived / locked / invitable
  state enums and member IDs on member-delta rows (also embedded in
  `dedup_key`), with no names, topics or message content. Member erasure
  deletes the member's rows by guild ID plus user ID.
- Event RSVPs and attendance/community facts, including attribution and compound
  voice-session/event keys (`event_rsvps`, `community_facts`). Scorecards normally
  store aggregates; the erasure plan also checks their serialized payloads.
- LFG posts, creators, role slots and member signups; feed configuration, creators
  and delivery state. Sticky bodies and their creator/editor IDs are stored. Scheduled messages store creator/editor IDs and are erased with them.
- Moderation, announcements, automation and operational audit rows, actor/target
  IDs, reasons and metadata. Some legacy-compatible payload columns are arbitrary
  TEXT, not database-validated JSON. The event-store capture (`event_store`:
  stage-instance, interaction-observe and audit-log-entry rows with IDs and
  enum fields only, no free text, plus invite rows with the code hash and the
  inviter ID and scheduled-event rows with IDs and status/entity-type enums)
  stores actor/target/user/inviter member IDs, erased as any one of them.
- Automod violation counts and once-per-message ledgers store member and message
  IDs and filter names, never content (`automod_violations`,
  `automod_processed_messages`). Per-delivery replay claims may keep a matched
  author ID (`automod_delivery_claims.matched_author_id`). Settled claims
  (result recorded) are erased with the member; an unsettled claim (in flight,
  started, counted or released without a result) is a replay guard and makes
  erasure refuse until it settles, so a gateway retry cannot sanction twice.
- Starboard reactions store guild, source-message and starrer IDs, plus per-user
  gateway-session hashes and sequence high-water marks (`starboard_starrers`,
  `starboard_reaction_cursors`). Remove tombstones prevent replay resurrection;
  both tables are covered by member erasure and departed-guild purges. No message
  text, author names or avatar URLs are persisted by the highlights runtime.
  Erasure removes the erased member's replay watermark along with their stars.
- Self-role audit and panel claims store member IDs, role effects, lease/recovery
  state and outcomes. Settled member records are covered; unresolved role effects
  or active leases refuse erasure rather than discard reconciliation evidence.
- One moderation ledger row is retained through erasure: a channel request an
  operator reconciled and released (`moderation_idempotency`, `done` /
  `operator_released`). Its Discord effect was never proven, so the row is the
  replay fence that keeps a delayed delivery of the old key from repeating the
  mutation. It retains guild/key scope, action, terminal state/outcome, generic
  result and timestamps. The caller-supplied key may itself be a member snowflake;
  if so, that identity-bearing key remains in this tombstone after erasure because
  changing or deleting it could let delayed delivery repeat the uncertain
  mutation. Release replaces both the obsolete ownership token and actor-derived
  request hash with fixed non-identity `operator_released` markers; the original
  token and hash are redacted from inspection and audit snapshots. Linked
  `moderation_audit` rows, including matching keys and free-text reasons, remain
  covered by the erasure manifest.
- Internal action/replay ledgers store actor/target/resource IDs and outcomes,
  with hashed keys/nonces/event identities. Settings and immutable settings audit
  store operator attribution and JSON policy configuration.
- Aggregate counters/probes, gateway checkpoints and shared role/event
  configuration also exist, but are not member activity records. Game-activity
  analytics (`game_activity_guild_aggregates`: per-guild per-game play/in-voice
  millis plus session counts, flushed hourly from the voice runtime's
  in-memory collector) belongs to this class: guild-keyed rows with no member
  column, gated before every persist by the same per-user opt-out and the
  presence toggle above, so opted-out members never persist and a disabled
  guild drains and drops. The gate read and the upsert share one serializing
  transaction holding the suppression table, so an opt-out landing mid-flush
  either filters out of that flush or waits for its commit. The voice toggle
  is independent: with voice off but
  presence on, play aggregates still persist while voice-derived `in_voice_ms`
  stays 0 (voice spans drain and drop). The table is not a replay fence and
  holds no fence rows; member erasure and opt-out purges have no row to delete
  there. Stale rows do expire by age on the retention cadence (see
  [Retention](#retention-and-explicit-exceptions)): aggregates whose
  `updated_at` is older than the horizon are deleted per guild.
  Operators read these rows through the [game-activity aggregate
  report](game-activity-report.md).

The authoritative, ordered deletion manifest is
[`member_erasure_plan.json`](../crates/cutover/src/member_erasure_plan.json).
Each row matches the guild **and** any covered identity, not just the main subject
column. A row matching several columns is counted/deleted once. Known serialized
metadata and compound identifiers use digit-boundary matching for the supplied
snowflake, which also handles malformed TEXT JSON and nested numeric/string IDs.
It is intentionally conservative: an unrelated standalone numeric value equal
to that snowflake also matches. This is not a general personal-data recognizer:
encoded/hashed identities, names, arbitrary prose without the ID, or a novel
identity-bearing field require source review and an explicit manifest update.

## Member opt-out (per-user, per-guild)

**For members:** you can opt out of analytics persistence for one server
(guild) at a time. Opting out in guild A changes nothing in guild B.
When your opt-out runs, the bot:

1. Deletes your event-store rows for that guild: your `events` rows
   (including rows that merely mention your ID in serialized metadata, with
   the same guild+identity matching as member erasure), your `members`
   projection row, any invite-snapshot rows attributing invites to you, and
   your `community_facts` rows (message, voice and join facts, matched by
   actor plus metadata/source-event/idempotency-key like member erasure).
2. Installs a suppression entry for your (guild, member) pair. From then
   on, every gated write seam drops future per-user persistence for that
   pair: event writes, activity/bot-flag upserts, onboarding prompt and
   selection rows (an opted-out member is not sent welcome prompts, because
   sending one would persist per-user state), community-facts writes
   (message facts follow the activity switch, voice facts the voice switch,
   join/rules facts are suppression-only), invite-attribution writes
   (invite use counts keep counting, with the inviter field blanked), and
   the standalone capture's counter baseline. A join attributed to your
   invite keeps the joiner's row but with your inviter field blanked.
   Anonymous aggregate counters keep counting throughout.
3. Writes exactly one audit receipt: the guild/member scope plus the
   actor label and the database timestamp.

Moderation, audit, XP/level, ticket, voice-room and settings rows are not
part of an opt-out: they serve safety, delivery and accountability, not
analytics. A member who needs full removal should request erasure (see
Procedure below), which is a separate reviewed operation.

Opting back in removes the suppression entry only; it does not restore
already-purged rows. Afterwards the pair persists exactly as before, and
the re-enable itself writes no purge receipt (the receipt table records
purges, one row per purge).

Members run this themselves with `/privacy opt-out` (`opt-in` to resume,
`status` to check). The command acts only on the invoker in the invoking
guild and writes the receipt with the fixed `member:self-serve` actor label.
A server operator can still execute an assisted opt-out through the reviewed
`opt_out_member` service with an explicit operator actor label, against a
disposable/staging binding first per the rollout gates. Either actor label
carries attribution only: never put a member ID, content, count or secret
in it.

Opt-out and opt-in serialize per `(guild, member)` in the privacy service,
including assisted operator calls. The transaction takes a pair-scoped
advisory lock before checking suppression or taking the existing purge
locks (5-second lock timeout, 15-second statement timeout). Concurrent
opt-outs produce one purge and receipt; already-suppressed calls take no
purge lock and write no receipt. A first opt-out with no stored activity
still installs suppression, writes its receipt and gets the opted-out reply.

For member self-serve, the service checks the existing 10-minute receipt
cooldown under that same pair lock, after suppression. Already-suppressed
taps get the already-opted-out reply, not a cooldown reply. A recent receipt
on an unsuppressed pair gets the cooldown reply without reinstalling
suppression or writing a receipt. Replies use the service's explicit
outcome, not a preflight read or a zero-row purge count. Assisted operator
calls remain exempt from the member cooldown.

**Policy note (verified 2026-10-12):** the live Discord Developer Policy
now lives on Discord's developer support help center (the old
`discord.com/developers/docs/...` URL redirects there). A direct fetch
from this sandbox on 2026-10-12 was blocked after the redirect chain
(301, then 307, then 403 bot detection), so this document makes no claim
about the policy's current text. Before citing Discord policy in a member
answer, open the live page and quote what it says that day.

### Staging verification read (fixture pairs, CI route)

The manual `staging-privacy-verify-read` workflow reads all-time per-pair
counts for one fixture member in one staging guild, without handing any
agent a database credential. The dispatcher chooses only the guild alias
(`g` for TWO Staging, `h` for the second guild); the script maps the alias
to its pinned guild and refuses anything else. The `h` pin travels the
normal config path: until `STAGING_PRIVACY_VERIFY_H_GUILD_ID` is set,
recorded and verified not-live, every `h` read refuses. The fixture member
comes only from the `STAGING_FIXTURE_MEMBER_ID` environment secret, never
from an input, so a dispatch cannot aim the read at another member.

The read-only login `TWO_BOT_STAGING_PRIVACY_RO_DATABASE_URL` must name
role `two_bot_privacy_verify_ro` on database `two_bot` at the host pinned
in `STAGING_PRIVACY_VERIFY_EXPECTED_HOST`. The role holds column-limited
`SELECT` on exactly six tables, covering every column the queries read:
`events` (`guild_id`, `member_id`, `metadata`, `idempotency_key`),
`members` (`guild_id`, `member_id`), `invite_snapshots` (`guild_id`,
`inviter_id`), `community_facts` (`guild_id`, `actor_id`, `metadata`,
`source_event_id`, `idempotency_key`), `privacy_opt_outs` (`guild_id`,
`member_id`), and `privacy_opt_out_audit` (`guild_id`, `member_id`,
`actor`, `opted_out_at`, `id`; `actor` and `opted_out_at` are projected,
`id` orders the receipts), and nothing else. Every statement runs in a
`READ ONLY` transaction with a 10 s
statement timeout after a `current_user`/`current_database()` identity
probe.

The six equality-count queries project `COUNT(*)` only. Two sweep queries
apply the reviewed `member_erasure` manifest predicates for `events` and
`community_facts` (the only purge tables matching beyond direct ownership)
restricted to non-owned rows, and likewise project only counts: a zero
sweep proves the purge touches only directly owned rows. One capped receipt
query returns actor labels plus `opted_out_at` (at most 10 rows plus an
overflow witness; the receipt count stays exact). Every count above
1,000,000 fails closed: a single fixture pair cannot legitimately reach it,
so a larger count means the predicates matched beyond the pair. The
15-minute window of the events-smoke read does not apply here; purge
verification needs all-time counts.

The run uploads `staging-privacy-verify-read-<run_id>.json` (14 days):
guild alias, collection timestamp, the six counts, the two sweep counts,
and the capped receipt rows. Only the exact identity-free actor labels
`member:self-serve` and `operator:fixture-check` may be published. Any other
actor fails the whole read without an artifact or summary, even if it has
a valid service shape or occurs only in the overflow witness. Labels are
not normalized or replaced. The independent artifact validator enforces
the same closed allowlist without needing member or guild bindings. Member,
guild pins, sources and metadata never reach the artifact, the log or the
step summary.

## Retention and explicit exceptions

The retention module (`two-bot-cutover::retention`, migration `0438`)
applies rolling nominal windows when invoked: raw presence/voice rows use a
30-day window, the audit-log cache uses 45 days, and user deletion requests
complete in under 72 hours with a receipt unless a replay guard is unresolved,
in which case the request stays pending with no receipt and is reported as
deferred (with overdue requests older than 72 hours counted separately).
Long-term aggregates (counts,
streaks) are never time-purged, with one exception: game-activity guild
aggregates (`game_activity_guild_aggregates`) expire by `updated_at` age on
the retention cadence (default 90 days, `TWO_GAME_ACTIVITY_AGGREGATE_RETENTION_DAYS`
accepts 1..=36,500 days and any missing, malformed, or out-of-range value
falls back to 90 days, only when `TWO_RETENTION_MODE` is exactly
`execute`; dry runs count the visited page's full stale rows without deleting).
Each production pass visits at most 32 candidate guilds and deletes at most
500 rows per visited guild (test builds page at 4 x 10 to keep fixtures
meaningful): tens of single-statement deletes per tick, far inside the
120-second attempt budget, for at most ~16k rows per 15-minute tick
(~1.5M/day). Each per-guild delete runs with `lock_timeout = '5s'` / `statement_timeout = '15s'`
(plus a 15 s pool statement timeout), so a lock held by a stuck session fails
that guild fast as an isolated retryable error instead of blocking the pass
until the supervisor cancels it and pins its pooled connection. The cursor is
written per guild immediately before that guild's delete, so a failed or
cancelled pass still advances past the last attempted guild and the next pass
reaches the later guilds on the same page, even when the whole fleet fits on
one page; `guilds_deferred` counts
eligible guilds after the visited page (zero only at the end of the eligible
list), so backlog lag is observable via `guilds_deferred`/`stale_rows` with no
per-guild labels. Dry-run totals are a same-predicate upper bound per pass
(execute deletes at most the per-guild batch), not an exact future-delete
promise under concurrent writes. After
expiry a returning game's totals restart from zero with no marker. Apart
from that window, a departed guild's rows go only through
the guild-purge queue keyed on a true guild removal (a rejoin cancels the
queued purge). The live gateway invokes the queue/cancel paths on real
`GUILD_DELETE`/`GUILD_CREATE` dispatches (outage payloads never queue; an
available rejoin cancels before the next run), and the supervised retention
job runs on a bounded schedule: a dry run that deletes nothing unless
`TWO_RETENTION_MODE` is exactly `execute`. No other automatic age-based
purger is installed: do not claim a live 30/90-day TTL beyond the windows
this job enforces. Every run reports per-class metrics, with partial runs
(deferred/overdue deletions, failed guild purges) surfaced as warnings that
stay retryable. The existing `presence_probe`, funnel `events` and
`operational_audit_log` cutoffs are evaluated at each successful run; scheduling
or database failures can retain those rows beyond their nominal windows until a
later successful run. The cadence headroom below is specific to `event_store`.

The authenticated erasure intake (`two-bot-cutover::deletion_intake`) is wired
through `/privacy delete`, `/privacy deletion-status` and `/privacy deletion-receipt`.
The handler derives the guild from `interaction.guild_id` and the requester from
`actor_id(interaction)`; no command accepts a target member. Requests bind to
that pair and the interaction id. Fresh requests acknowledge as pending;
idempotent retries reuse the request without queueing it again and direct the
member to status rather than claiming a new pending queue. Requests persist to
the same deletion-request ledger the scheduler drains — never the narrower opt-out
purge. Status and completion-receipt reads use the same authenticated pair plus
the request id, without the interaction token. Receipts name the erasable rows
in the member erasure plan, disclose the retained `retention_deletion_requests`
request/receipt ledger and `privacy_opt_out_audit (member_id, actor)`, and do not
claim the guild-purge queue. When the database is behind the binary (an unapplied migration or grant),
the gateway acks dispatches whose queue write hits the schema gap instead of
crash-looping, with one exception: a missing grant (`42501`) on an available
rejoin holds the cursor (the gateway fails the dispatch and replays it) until
the ACL plan is applied, because the queue may hold a pending purge for that
guild. A missing queue table (`42P01`) on a rejoin is still skipped — an absent
table holds nothing to cancel — and
a skipped true guild removal is logged at error level with the guild id
(`retention purge skipped; schema gap, operator replay required`), increments
the pollable `retention_replay_needed` counter on `/metrics`
(`two_bot_retention_replay_needed_total`), and must be replayed by the
Operator once the migration lands — a nonzero counter means at least one
skip since the last restart and is only an early hint: the counter resets
on process restart and carries no guild id, so the error log line
(`operator replay required`) stays the source of truth for which guilds to
replay. Always search that line over the whole schema-gap window, then run
the same insert the live path runs:
`INSERT INTO retention_guild_purge_queue (guild_id)
VALUES ('<guild id>') ON CONFLICT (guild_id) DO NOTHING;`
Conditional: replay only if no `GUILD_CREATE` for that guild was acked after
the `operator replay required` line — a later `retention lifecycle skipped;
schema gap, nothing to cancel` warn for the same guild means it rejoined, so
skip the insert while the guild is live (replaying it would queue a live guild
for the next `execute` run). Run the default dry run first and confirm the
guild is still gone before executing.

The append-only event-store capture (`event_store`) carries one narrow,
documented retention exception: the same retention job expires its raw
presence and voice captures (`PRESENCE_UPDATE`, `VOICE_STATE_UPDATE`,
`VOICE_SERVER_UPDATE`) at 30 days and its cached Discord audit entries
(`GUILD_AUDIT_LOG_ENTRY_CREATE`) at 45 days, selected by `event_time`
(never the insertion time, so a backfill or replay cannot extend old
data's lifetime) and only in `execute` mode. For these event-store captures,
the 15-minute supervised retention lifecycle reserves one cadence as cutoff
headroom and selects rows at or before `next_sweep_deadline - retention_window`
(inclusive UTC instant),
so those captures can expire up to 15 minutes before their nominal maximum age.
Each run commits at most one globally capped batch of 1,000 rows across all
three capture classes, ordered by oldest `event_time`; later runs continue the
backlog, allowing user-deletion and guild-purge work to proceed each cadence.
Dry-run reports the full eligible capture backlog, not just one batch. If a
batch times out, that batch rolls back; batches committed by earlier runs stay
committed, and the remaining backlog is retried on a later cadence. This is not
a claim that an arbitrarily large or
continuously growing backlog can be drained within one cadence; steady-state
expiry depends on the supervised job keeping up with arrivals. Capture writers
stay insert-only and no other `event_store` family has an age TTL; those rows
persist until erasure. The per-user purge path is this procedure plus one manifest entry per
landing slice, deleting by guild plus user; rows without a user id purge per
guild (and per message or thread where the columns exist) instead. The
capture fence is ids, timestamps and enums only: no message bodies and no
custom free text are ever stored. See the
[event-store wiring note](event-store.md) for the per-slice fence table,
the backfill-no-replay rule and the read-only staging verify steps.

The manifest explicitly retains these control-plane records:

| Record/columns | Reason and operator follow-up |
| --- | --- |
| `guild_settings.updated_by`, `value` | Active configuration and its attribution, including staff/raid/test actor-ID lists. Blindly removing a policy can weaken or broaden safety/permission gates. Use a separate reviewed configuration change if an ID must leave policy. |
| `guild_settings_audit.actor`, `old_value`, `new_value` | Immutable configuration history; the database refuses DELETE, UPDATE and TRUNCATE, even a zero-row DELETE. No trigger disabling or audit bypass is permitted. IDs in prior/current policy remain here. |
| `audit_kill_switch.engaged_by` | Global incident-control attribution, with no guild scope. Member erasure never disengages or rewrites the halt. |
| `member_erasure_audit.actor` | Minimal operator accountability, retained independently of member activity. It is not the erased subject field. |
| `admin_api_tokens.holder_id`, `issued_by` | Named human/AI credential principal (which may identify a member) and issuing-operator attribution. Guild-local activity erasure must not rewrite holders or silently change cross-guild grants. Separate authorized credential revocation/retention disposition is required; see the [registry contract](admin-registry.md). |
| `privacy_opt_outs` (guild, member, time) | Guild-scoped opt-out suppression tombstone. Deleting it would silently resume per-user persistence the member refused. Retained until the member opts back in; member erasure never removes suppression. |
| `privacy_opt_out_audit` (scope, actor, time) | One receipt row per opt-out/purge, same accountability class as `member_erasure_audit`. The subject appears only in the scope columns, never in the actor label. |
| `guild_analytics_settings` | Per-guild ingestion switches (presence/activity/voice). Guild configuration, not member activity; no member IDs stored. |
| `moderation_idempotency` rows with `done` / `operator_released` | Indefinite replay fence for an uncertain Discord mutation. It retains the guild-scoped caller key, which may itself be a member snowflake and therefore remains as identity-bearing data, plus action, fixed `claim_token` and `request_hash` markers, generic result and timestamps. The obsolete ownership token and actor-derived request hash are replaced, and the original hash is redacted from inspection/audit snapshots. |

These exceptions currently have **no enforced expiry**. This CLI is not a claim
that all personal data is gone. A request involving retained policy/audit data
needs a separate security/privacy disposition, not a silent exception or bypass.
Guild-local erasure also does not delete NULL-guild internal records, global
hashed nonce/event replay guards, other-guild activity, existing backups,
external logs, or Discord messages/attachments.

## Guild analytics toggles

Operators can disable analytics ingestion per guild and per family with one
row in `guild_analytics_settings` (see the `analytics_settings` /
`set_analytics_settings` service calls). A guild with no row has every
category enabled, matching the behavior from before the toggles existed.
Each switch is independent: disabling presence never touches activity or
voice.

| Family | Switch | Gated event types |
| --- | --- | --- |
| Presence | `presence_enabled` | `presence_update` |
| Activity | `activity_enabled` | `first/second/third_message`, community `message_created` facts |
| Voice | `voice_enabled` | `voice_session_start/end`, `first_voice_session`, community `voice_session_started/ended` facts |

Every other event type still persists: joins, leaves, gate clears,
onboarding prompts, invite clicks and routing rows serve
safety/moderation and onboarding delivery, not analytics. Membership
lifecycle rows in particular stay unconditional, so a disabled analytics
category can never blind moderation to a join or a leave. Presence updates
already have no durable funnel effect at the gateway (they feed only
in-memory voice-room name facts); the presence switch additionally drops
any `presence_update` row a backfill or replay would otherwise write.

Gated seams: the cutover event writer (`record_event`, with `record_earliest`
and the backfill/community replays that call them), the runtime funnel
store, the gateway batch commit, the community-facts writer (`record_fact`,
including the leveling-runtime flush and the RSVP host check-in, which is
suppression-only like other lifecycle facts), invite snapshots (suppressed
inviters keep the aggregate use count with a blanked inviter field, in the
standalone capture's counter baseline and the backfill baseline alike, which
share one writer), the onboarding writers (including the prompt guard, which
re-gates before the insert for opt-outs that land while the welcome is in
flight), `touch_activity` / `mark_bot`, and the inactivity sweep (suppression-only:
a suppressed pair is skipped inside the per-member transaction, so the sweep
never recreates the purged `member_inactive` row). Every gated writer serializes
with concurrent opt-outs (a shared table lock, so a write that passes the
gate can never land after the purge), and `member_join` rows strip a
suppressed inviter's `inviterId` from metadata while keeping the joiner's
own row. Gateway activity timestamps carry their originating family
(message/activity versus voice) through the timestamp buffer, and the
batch commit applies the matching guild toggle inside the same
transaction before the monotonic `last_active_at` upsert, so a disabled
family's touch creates or advances nothing and a newer disabled stamp
can never replace the allowed family's timestamp; the runtime funnel
store's `touch_activity` honors the same toggle. The standalone
`cutover::db::{touch_activity, mark_bot}` helpers and the backfill
binaries that call them stay suppression-only (they carry no family
signal), which is the remaining residual for a follow-up, not silent
per-user analytics. Duplicate
reconfirmations for suppressed pairs advance nothing: the gate runs before
the observation-maximum path too.

Backups follow the existing [backup runbook](backup.md): retention is measured
in retained snapshots (default 14 newest), **not 14 days**. Historical snapshots
can contain erased records. Restrict backup access and prevent reintroduction
through a restore: re-apply separately authorized erasure requests before opening
restored data to users. The erasure receipt deliberately does not retain a subject
list. Backup destruction/rotation or a new retention policy is not authorized by
this CLI.

## Procedure

1. Verify an authorized member request, guild and exact Discord snowflake. Use an
   approved operator database identity with the required read/delete/lock rights
   and receipt-insert access. The command uses only `TWO_DATABASE_URL`; it never
   falls back to another credential or applies migrations. Provision migration
   `0410_member_erasure_audit.sql` through the normal reviewed migration process.
2. Pause the guild's ingestion, command/internal-action workers and replay queues
   through the existing operational procedure. Erasure is not a suppression or
   opt-out tombstone: live activity/backfills/replays can recreate data after
   commit. Resolve matching pending/unknown action intents through their existing
   reconciliation path first; never relabel them completed just to run erasure.
3. Dry run with the authorized database binding already supplied securely:

   ```sh
   two-bot erase-member --guild <guild-id> --user <member-id>
   ```

   Output is `table<TAB>count`, then `DRY RUN: no changes`. No audit row is written.
   Review counts and collateral scope: deleting a creator's LFG post also removes
   its roles and all associated signups; deleting a creator's feed removes its
   deliveries. Another member's unrelated post/feed/activity is preserved. A
   serialized row mentioning the subject is removed even if its main actor is
   someone else. Counts include these child rows explicitly, not hidden cascades.
4. After the deletion authorization and count review, set `TWO_ERASURE_ACTOR` to
   the accountable operator identity (nonempty, at most 128 characters, no control
   characters; never put a secret or the subject's content there), then run:

   ```sh
   two-bot erase-member --guild <guild-id> --user <member-id> --execute
   ```

   Exit 0 plus `ERASURE COMMITTED` is success. Argument refusal is exit 2;
   connection/transaction failure is exit 1 with redacted details. Any SQL,
   schema-coverage, replay-guard, count or receipt failure rolls back all deletions.
   A commit transport error has an uncertain outcome: inspect a fresh dry run
   before retrying; do not infer rollback solely from a failed client response.
5. A fresh dry run must report zero in every covered table. A second execute also
   reports zero and exits 0 (and writes its own minimal operator receipt).
   Resume ingestion only after deciding whether future activity is authorized.

Dry run uses one read-only repeatable-read snapshot. Execute locks the covered
and receipt tables in a consistent order before counting and deleting in **one
transaction**, with a 5-second lock timeout and 15-second statement timeout.
Execute reports the same per-table counts as dry run when no data changed between
runs; there is no cross-process snapshot guarantee. Locks briefly affect unrelated
guilds sharing these tables. Maintenance/quiescence is required, not a licence to
run a disruptive command against an active production service.

### Erasure receipt and output privacy

The transaction inserts exactly **operator actor + database timestamp** into
`member_erasure_audit`. There is no subject ID/hash, guild, request key, content,
counts, or reason in that table. stdout contains table names/counts only; errors
never print SQL, IDs or database URLs. The actor value is operator-supplied
attribution, not independently authenticated by this command; access control is
provided by the approved execution and database identity. Do not put subject IDs
in the actor label or capture credential-bearing environment variables.

## Coverage regression and acceptance tests

The integration suite applies **all** embedded migrations to its own disposable
database through `two-bot-testsupport`, then queries `information_schema.columns`.
Member/user names (including new `*_user_id`/`*_member_id` columns), legacy actor,
inviter/target aliases and `*_by` attribution require a manifest entry or a
nonempty, column-specific retention reason. Stale entries also fail. Both runtime
modes refuse an incomplete visible schema. The schema check is a naming tripwire,
not semantic proof of arbitrary JSON/prose or tables invisible to the DB identity;
operators need read access to the complete application schema.

Fixtures seed two members in two guilds across every deletion-plan table and seed
retained settings/audit/control records. Tests prove equal dry/execute counts,
byte-equivalent unrelated survivors, no covered remaining references, idempotence,
minimal receipts, nested inviter/session identifiers, explicit child counts,
refusal of unresolved side-effect guards, and full rollback on a late receipt
failure. A synthetic migration adding a recipient/observer `*_user_id` is a
negative coverage probe. CI's existing workspace integration step runs this suite
with its ephemeral Postgres service; configured test DB errors never silently skip.

On the controller, use the approved cache wrapper (never direct compiling Cargo):

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_ci \
  python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test member_erasure
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_ci \
  python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test privacy_opt_out
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_ci \
  python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test privacy_service
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot erasure_cli
cargo fmt --all -- --check
```

The `privacy_opt_out` suite is the failing-first acceptance contract for
opt-out purge plus suppression plus toggles plus the audit receipt; the
`privacy_service` suite covers the `opt_out_member` / `opt_back_in` /
analytics-settings service calls. Neither suite may be edited to make the
implementation pass: fix the implementation instead.

The bootstrap database must already exist; the fixture creates/drops only its own
unique test database. Never use a staging/production binding for these tests.
