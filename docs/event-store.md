# Event store (P1 wiring note)

Engineering note for the append-only gateway capture table (`event_store`).
Read the status line first: the capture slices land incrementally, and each
landing slice registers its columns across the migration lock, the database
role matrix, the backup inventory, the member-erasure plan and the docs.
Until a slice's registration lands, this note describes the wired end-state
each slice must reach, not current `main`. It changes no behavior, installs
no retention policy and grants no operator command. The privacy dispositions
live in [privacy.md](privacy.md); the operator responses live in the
[operations runbook](runbook.md#event-store-capture-gaps-and-lag).

## Privacy fence: ids, timestamps and enums only

Every slice stores identity plus timestamps (`event_type`, `guild_id`,
`user_id`, `event_time`, `received_at`, `dedup_key`) and small enum, flag or
id-reference columns. Message bodies and custom free text are never stored:
each slice's acceptance suite carries a tripwire with sentinel strings that
must appear in no stored row.

| Slice | Stored besides identity + timestamps | Never stored |
| --- | --- | --- |
| Message create | Message/channel/guild/author ids only (`message_id` plus the shared identity columns) | Content/body, embeds, attachments, username, custom-status text |
| Member add/remove/update | Membership deltas only: `premium_since`, `nick_changed` flag, role-id list | Bio, about, username, nickname |
| Presence | Status and activity-TYPE enums plus application id | Custom-status text; activity name, details, state, URL, emoji |
| Voice state/server | Channel/session ids plus mute, deaf, self-mute, self-deaf, self-video, self-stream and suppress flags | Audio, transcripts, voice tokens, endpoints |
| Message update/delete + reactions | Channel/message ids plus emoji id/name reference (a unicode reaction persists as a codepoint reference such as `U+1F44D`, never as text) | Content, embeds, attachments, username |
| Thread lifecycle + member deltas | Thread/parent ids plus archived, locked and invitable flags | Thread name, topic, message body, username |
| Invite + scheduled-event | Invite-code hash and entity-type enums | Raw invite code (in no column, including `dedup_key`), description, name, location, username |
| Stage + interaction-observe + audit-log | Privacy-level, interaction-type and action-type enums | Topic, bearer token, change detail, options, reason |

Interaction capture is observe-only: the bearer token stays in the live
dispatch for the acknowledgement path and is never stored.

Resume-safe redelivery is first-wins: every insert runs
`ON CONFLICT (dedup_key) DO NOTHING` against the single shared unique index
on `dedup_key`, so a gateway resume redelivery inserts nothing. Later slices
add columns with `ADD COLUMN IF NOT EXISTS` and reuse that index rather than
adding a second unique constraint.

## Retention and erasure wiring

The existing supervised retention job applies age expiry only to the raw
presence/voice capture families and cached Discord audit entries described in
[privacy.md](privacy.md#retention-and-explicit-exceptions). It selects by
`event_time`, runs deletes only in `execute` mode, and processes one globally
capped batch per cadence. The job defaults to dry-run; its report shows the full
eligible backlog, while an execute run deletes at most 1,000 rows. Cutoff
headroom and the limits of the batch guarantee are documented in the privacy
policy section. This does not add age expiry to other `event_store` families,
which remain until authorized erasure or per-guild purge. Do not claim a
30/90-day TTL for those unrelated families.

The per-user purge path is the existing member-erasure procedure in
[privacy.md](privacy.md#procedure): each landing slice registers an
`event_store` manifest entry that deletes by guild plus user, and the
schema-coverage tripwire refuses an identity-bearing column with no
manifest entry. Rows that carry no user id need a different key: bulk-delete
fan-out rows and thread-lifecycle rows purge per guild (and per message or
per thread where the columns exist) as plain operator `DELETE`s, not through
the member manifest. Until a slice's registration lands, erasing a subject
with that slice's rows needs a reviewed manifest update first.

## Operations

### Lag threshold

A `two_bot_gateway_missed_events_total` increase between keepalive samples
is a capture gap, not a backlog: the gateway never received those
dispatches, RESUME does not replay them, and no backfill can reconstruct
them. Handle it exactly like the existing missed-events alert in the
[runbook](runbook.md#alert-gateway-missed-events).

Capture lag is the spread between `received_at` and `event_time` per
`event_type` over a window. The `two_bot_event_store_consumer_lag_seconds`
gauge exposes the newest observed spread (`0` when unobserved), and the
`event_store_consumer_lag` Worker rule tickets when it stays above one
keepalive window (about 60 s) across three consecutive valid samples; the
staging verify query below remains the manual check. Treat such a spread as a
missed-events-equivalent: scrape the authorized `/ops/metrics` path,
correlate disconnects and recent deploys, and do not restart the container
to clear the counter. A replacement resets baselines without recovering
the gap.

### Backfill-no-replay rule

Store writes are insert-only. A backfill or repair run may only insert rows
with fresh `dedup_key` values; it must never `UPDATE` or `DELETE` existing
rows (history is not rewritten) and must never replay an uncertain Discord
effect. A capture row proves observation, not that a Discord mutation
happened: never drive moderation, role, channel or announcement writes from
store rows, and never re-issue a mutation because its capture row is
missing. Quiesce capture before member erasure per
[privacy.md](privacy.md#procedure): live activity, backfills and replays
after commit can recreate erased rows, and a backfill that would re-insert
erased data needs fresh authorization first.

### Staging verify steps (read-only)

Run these through the existing authorized staging read path. They write
nothing: never migrate, reset or seed staging for verification.

1. Count rows per `event_type` over the soak window and confirm every
   expected family appears with none dominating beyond its dispatch share.
2. Confirm `dedup_key` uniqueness holds (no duplicate keys) and that a
   redelivered dispatch inserts nothing.
3. Confirm the forbidden columns are absent via `information_schema.columns`
   and that sampled newest rows hold ids, timestamps and enums only.
4. Report the `received_at - event_time` spread per `event_type`; a spread
   above one keepalive window across consecutive windows is a lag finding,
   not a pass.
