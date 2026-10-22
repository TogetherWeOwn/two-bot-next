# Suggestions triage box (design spec)

Status: **design spec, no implementation**. This is a new surface with no
legacy parity source: the only `suggest` hits in the tree are voice-assistant
internals. Builds follow after owner alignment; each build slice is one PR
(see below). Out of scope here: any code, ticket-system changes, dashboard UI.

## Goals

- Members submit suggestions through one bot-owned flow; every suggestion is a
  durable row with a stable Discord message.
- The community votes up/down with one-vote rules; staff triages each item to
  `accepted`, `declined` or `implemented`, with an optional ticket handoff.
- Anti-spam and rate limits hold from day one; privacy follows
  [the member-data inventory](privacy.md): Discord IDs only, erasure wired.

Non-goals: weighted/staff-weighted votes, suggestion threads and discussion,
automatic ticket creation, dashboard UI, legacy import.

## Submit flow

1. `/suggest text:<text>` slash command, configured-guild fenced and default
   off (same posture as the announcements fencing in [the LFG
   integration](lfg-port.md)). No message-scrape submission in v1: a command
   gives a clean validation and acknowledgement point.
2. Validation, all before any durable write: trim ECMAScript whitespace;
   length cap 1,000 UTF-16 code units without splitting a surrogate pair;
   suppress `@everyone`/`@here` and user/role mentions in the stored text;
   refuse exact-text duplicates from any member within the last 24 hours with
   a link to the existing suggestion.
3. Per-member admission, serialized in one advisory-lock transaction like the
   ticket reservation in [the ticket runtime](tickets.md): at most 3 open
   suggestions per `(guild_id, author_id)`, and one submission per 10 minutes
   measured from creation. Over either bound the command replies ephemerally
   and writes nothing.
4. Persist the row, then post the embed to the configured suggestions channel
   with status OPEN and the vote buttons. A deferred ephemeral acknowledgement
   precedes SQL and REST, finished through the shared `ActionExecutor`; there
   is no private dispatcher or HTTP client. A crash between row insert and
   posting leaves `message_id` unset for recovery to repair, mirroring the
   LFG nonce-recovery posture (uncertain history keeps durable state).

## Voting

- Mechanism: two buttons on the suggestion message, stable component IDs
  `two:suggest:upvote` and `two:suggest:downvote`, registered through the
  shared router. The executor resolves the suggestion by the interaction's
  guild + channel + message, never from an interaction-supplied suggestion ID
  (same rule as ticket claim/close resolving `by_channel`).
- One-vote rules: one vote per `(suggestion_id, voter_id)`. Pressing the
  opposite button switches the vote; pressing the same button again removes it
  (toggle). The author votes like any other member; there is no implicit
  author vote. Staff votes weigh the same as member votes in v1.
- Each vote upserts `suggestion_votes` and adjusts the denormalized counters
  in one transaction under a per-suggestion advisory lock, so concurrent votes
  cannot lose counts. Triaged suggestions refuse votes: the status check and
  the vote write share the same row lock.
- Vote buttons are admitted through the member's shared interaction budget;
  over budget the press gets one ephemeral busy reply and no write, mirroring
  ticket component admission. Renders are mention-suppressed; a refresh
  failure says the saved vote remains.

## Thresholds

- Trending: net score (up minus down) at or above the configured threshold
  (default 10) marks the suggestion trending: the embed gains a trending flag
  and staff get one notification per suggestion (no repeat pings as the score
  climbs further).
- Thresholds never auto-accept or auto-decline: they drive staff queue
  ordering (trending first) and the trending flag only. Every
  threshold-driven terminal state is a deliberate staff action. This keeps
  the first build reviewable and avoids silently discarding minority
  suggestions. The only automated terminal transition in v1 is the 90-day
  retention stale-close below, which is a storage bound, not a verdict.
- The threshold is a per-guild setting with the default above; changing it
  re-evaluates open suggestions on the next vote, not in a backfill.

## Staff triage actions

- Actions: **accept**, **decline** (requires a short reason, max 280 chars),
  **implement** (terminal, for accepted items that shipped). All three lock
  voting, update the embed with the outcome, and post an outcome reply naming
  the triaging staff member's decision (reason shown for declines).
- Authorization mirrors ticket claim/close: the configured staff role,
  `ManageGuild`, or `Administrator`, resolved from the invoking member, never
  from command options.
- Ticket handoff: accepting staff may attach an existing ticket by channel;
  the store records the link after a guild-fenced ticket lookup and the
  suggestion embed gains the ticket link. v1 does **not** auto-create tickets
  and changes nothing in the ticket flow; the handoff is a stored reference
  plus a link. Detaching is a staff action that clears the reference.
- Triage is idempotent per suggestion: re-accepting an accepted suggestion is
  a no-op reply, and terminal states never reopen except through an explicit
  staff reopen action (open again, votes preserved).

## Anti-spam and rate limits

- Submit: one per member per 10 minutes, max 3 open per member (above).
- Votes: shared per-member interaction budget admission; toggle/switch writes
  are cheap single-row upserts, no extra cooldown in v1.
- Content: length cap, mention suppression, 24-hour exact-duplicate refusal.
- Members the guild has timed out or banned cannot submit or vote; the check
  reuses the live Discord member state at interaction time, not a cached copy.
- Runtime: bounded in-flight component executions per process (ticket-style
  admission with a `MAX_SUGGEST_COMPONENT_IN_FLIGHT` cap), label-free
  saturation telemetry, and no queue waiters beyond the shared lane bounds.

## Data model

Additive migration, new tables (no legacy names to preserve):

- `suggestions`: `id` (uuid PK), `guild_id`, `author_id` (nullable after
  erasure, see below), `channel_id`, `message_id` (nullable until posted),
  `text`, `status` (`open`, `accepted`, `declined`, `implemented`),
  `upvotes`/`downvotes` counters, `trending` flag, `ticket_id` (nullable
  reference to the linked ticket), `decline_reason` (nullable),
  `triaged_by`/`triaged_at` (nullable), `created_at`. The 3-open cap is a
  count check inside the advisory-lock transaction, with the lock as the
  arbiter; unlike the ticket max-1 rule, no partial unique index applies.
- `suggestion_votes`: `(suggestion_id, voter_id)` PK, `guild_id`,
  `vote` (+1/-1), `updated_at`. Guild-fenced on every read and write; rows
  cascade on suggestion delete.
- Status transitions are fenced in the store: only `open` rows accept votes
  or triage writes, and triage writes match the expected prior status so a
  late retry cannot overwrite a newer decision.

## Moderation and privacy notes

- Stored: Discord guild/channel/message/member IDs, the suggestion text
  itself, votes, triage outcomes and reasons. No names, no message history, no
  tokens, no endpoints. This extends [the member-data
  inventory](privacy.md), which each build slice must update.
- Retention: suggestion rows live until a stale-close pass retires them: open
  suggestions with no vote or triage activity for 90 days transition to
  `declined` with reason `stale`, keeping the text as the decision record.
  Staff may reopen. The purge runs on its own lane so slow Discord work cannot
  postpone retention cleanup, mirroring tickets.
- Member erasure: the member's votes are deleted and the affected counters
  recomputed in the same transaction. Authored suggestions still `open` are
  deleted with their votes; triaged suggestions are anonymized (`author_id`
  set NULL, text and outcome kept) to preserve the staff decision trail.
  Erasure removes database tracking only, never the Discord message, matching
  the inventory's scope.
- Attribution: the embed shows the author's mention while the row has an
  author; anonymized rows render "a former member". Triage outcomes always
  name the action and reason, never the voter's identity beyond counts.

## Ticket-command reuse notes

- Stable `two:suggest:*` component IDs through the shared router; resolve by
  guild/channel/message, never trust interaction-supplied IDs.
- Deferred ephemeral acks via the shared `ActionExecutor`; SQL before REST;
  refresh failure reports the saved mutation as remaining.
- Advisory-lock serialization for submit admission and vote upserts, modelled
  on the ticket reservation and LFG capacity locks.
- Ticket-style component admission bounds and label-free saturation telemetry
  for vote bursts.
- agent-testdb conventions for ignored store tests (fixed endpoint, unique
  schemas, per-test cleanup); see the ticket verification section.
- Privacy-inventory updates ride each slice, not a follow-up.

## Build slices (one PR each, dependency order)

1. **Domain, store, migration.** Pure validation (trim, caps, mention
   suppression, duplicate window), vote adjudication (toggle/switch),
   `suggestions`/`suggestion_votes` tables, guild-fenced store with
   advisory-lock vote upserts and fenced transitions, agent-testdb proofs.
2. **Submit and voting runtime.** `/suggest` registration (default off),
   admission, embed post, vote buttons, refresh, recovery for unset
   `message_id`. No staff actions yet.
3. **Staff triage and ticket link.** Accept/decline/implement + reopen,
   authorization, outcome replies, ticket attach/detach by channel lookup.
4. **Automation and privacy closeout.** Trending flag + staff notification,
   90-day stale close on its own lane, member-erasure wiring, inventory and
   runbook updates, staging enablement notes.

## Completeness self-review

- [x] Submit flow defined (command, validation, admission, post ordering).
- [x] Voting defined (buttons, toggle/switch one-vote rules, no implicit or
      weighted votes, locked after triage).
- [x] Thresholds defined (trending default 10, thresholds never auto-decide;
      stale close is a retention bound, not a verdict).
- [x] Staff triage defined (accept/decline/implement + reopen, auth, reasons,
      ticket handoff as stored link, no ticket-system changes).
- [x] Anti-spam/rate limits defined (submit cooldown + open cap, vote
      admission, content rules, timeout/ban check, runtime bounds).
- [x] Data model defined (two tables, counters, fenced transitions).
- [x] Moderation/privacy notes defined (IDs only, retention, stale close,
      erasure semantics, attribution).
- [x] One-PR build slices proposed in dependency order (4 slices above).
- [x] Ticket-command reuse notes recorded.
