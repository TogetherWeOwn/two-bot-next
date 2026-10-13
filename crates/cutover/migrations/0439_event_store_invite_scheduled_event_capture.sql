-- 0439_event_store_invite_scheduled_event_capture: append-only invite +
-- scheduled-event capture for event-store S7 (parent scope TOG-22849 slice 7).
--
-- Creates the append-only `event_store` table (if no sibling slice created it
-- yet) and adds the S7 invite/scheduled-event columns: identity + timestamps
-- (`event_type`, `guild_id`, `channel_id`, `event_time`, `received_at`,
-- `dedup_key`) plus the invite columns (`invite_code_hash`, `inviter_id`)
-- plus the scheduled-event columns (`scheduled_event_id`, `event_status`,
-- `entity_type` enums). Ids, code hash, timestamps and enum fields only: the
-- raw invite code is stored in no column, including `dedup_key` (only its hex
-- sha256 hash persists); no description column, no name column, no location
-- column, no username column, no raw-payload json/jsonb column (see the
-- FORBIDDEN_COLUMNS denylist in crates/discord/tests/event_store_s7.rs).
-- Resume-safe dedup is
-- `ON CONFLICT (dedup_key) DO NOTHING` against the shared UNIQUE index
-- below, with `dedup_key` shaped per event as
-- `INVITE_CREATE:<guild_id>:<channel_id>:<code_hash>:<event_time>` (DELETE
-- likewise), `GUILD_SCHEDULED_EVENT_CREATE:<guild_id>:<scheduled_event_id>:
-- <event_time>` (UPDATE/DELETE likewise) and
-- `GUILD_SCHEDULED_EVENT_USER_ADD:<scheduled_event_id>:<user_id>:
-- <event_time>` (REMOVE likewise); same-key redelivery inserts nothing and
-- the first row wins on replay. The store is append-only, so lifecycle
-- transitions land as separate rows.
--
-- Ordering: S7 may land before or after any sibling slice, so every step is
-- re-runnable (`IF NOT EXISTS`): the CREATE covers a missing table and the
-- ALTERs cover a table a sibling created first with a narrower column set.
-- Later slices must add their own columns with `ADD COLUMN IF NOT EXISTS` and
-- must reuse the shared dedup index name `idx_event_store_dedup_key` (via
-- `CREATE UNIQUE INDEX IF NOT EXISTS`) rather than adding a second UNIQUE
-- constraint, so the resume-safe key stays a single index.
--
-- Forward-compat nullability: `channel_id`, `user_id` and every S7 capture
-- column are NULLABLE so non-S7 rows (e.g. S1 message metadata, S2 member
-- rows, S3 presence rows, which carry no invite hash or scheduled-event ids)
-- can share the table, and so S7 rows insert cleanly when another slice
-- created the table first: invite rows carry no scheduled-event id or member
-- id, lifecycle rows carry no invite hash, inviter or member id, and user
-- deltas carry no channel id, invite hash or inviter. `guild_id`/`event_time`
-- stay NOT NULL because every slice to date binds them. `user_id` is created
-- NULLABLE here, and the explicit `DROP NOT NULL` below relaxes it when an
-- earlier slice (S2/S4) created the table first with `user_id NOT NULL`;
-- without that step their NOT NULL would refuse S7 invite and lifecycle rows.
-- No snowflake CHECK constraints on guild/channel/user/scheduled-event ids:
-- fixtures and early guilds carry short numeric ids, and a format check
-- would refuse them.
--
-- Erasure: invite rows are keyed by guild_id + inviter_id, user-delta rows by
-- guild_id + user_id, and lifecycle rows by guild_id + scheduled_event_id,
-- so per-guild purge and per-user erasure are plain DELETEs on those columns
-- (`inviter_id` is covered by the event_store manifest entry alongside
-- actor_id/target_id/user_id); retention windows stay with the retention
-- slice, not this migration. Roles/seeds: none; the table inherits the
-- database's default grants and carries no seed rows.
-- Rollback: `ALTER TABLE event_store DROP COLUMN IF EXISTS invite_code_hash`,
-- `DROP COLUMN IF EXISTS inviter_id`, `DROP COLUMN IF EXISTS
-- scheduled_event_id`, `DROP COLUMN IF EXISTS event_status`, `DROP COLUMN IF
-- EXISTS entity_type` (plus `DROP TABLE IF EXISTS event_store` solely when
-- this migration created the table and nothing else uses it).
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_store (
  id                 BIGSERIAL PRIMARY KEY,
  event_type         TEXT NOT NULL,
  guild_id           TEXT NOT NULL,
  channel_id         TEXT,
  user_id            TEXT,
  invite_code_hash   TEXT,
  inviter_id         TEXT,
  scheduled_event_id TEXT,
  event_status       INTEGER,
  entity_type        INTEGER,
  event_time         timestamptz NOT NULL,
  received_at        timestamptz NOT NULL DEFAULT NOW(),
  dedup_key          TEXT NOT NULL
);

ALTER TABLE event_store
  ADD COLUMN IF NOT EXISTS event_type         TEXT,
  ADD COLUMN IF NOT EXISTS guild_id           TEXT,
  ADD COLUMN IF NOT EXISTS channel_id         TEXT,
  ADD COLUMN IF NOT EXISTS user_id            TEXT,
  ADD COLUMN IF NOT EXISTS invite_code_hash   TEXT,
  ADD COLUMN IF NOT EXISTS inviter_id         TEXT,
  ADD COLUMN IF NOT EXISTS scheduled_event_id TEXT,
  ADD COLUMN IF NOT EXISTS event_status       INTEGER,
  ADD COLUMN IF NOT EXISTS entity_type        INTEGER,
  ADD COLUMN IF NOT EXISTS event_time         timestamptz,
  ADD COLUMN IF NOT EXISTS received_at        timestamptz,
  ADD COLUMN IF NOT EXISTS dedup_key          TEXT;

-- S7 invite and lifecycle rows carry no member id; relax user_id when an
-- earlier slice created it NOT NULL. Safe when already nullable (no-op) and
-- ordered after the ADD COLUMN above so the column always exists here.
ALTER TABLE event_store ALTER COLUMN user_id DROP NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_event_store_dedup_key
  ON event_store (dedup_key);
