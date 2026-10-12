-- 0428_event_store_presence_capture: append-only presence capture for event-store S3.
--
-- Creates the append-only `event_store` table (if S1/S4 has not created it
-- yet) and adds the S3 presence columns: identity + timestamps
-- (`event_type`, `guild_id`, `user_id`, `event_time`, `received_at`,
-- `dedup_key`) plus the presence enums (`presence_status`, `activity_type`,
-- `application_id`). Ids, timestamps and enum fields only: no custom-status
-- text column, no activity name/details/state/URL/emoji column (see the
-- FORBIDDEN_COLUMNS tripwire in crates/discord/tests/event_store_s3.rs).
-- Presence is the highest-volume family, so the insert path stays lean:
-- one row per update, `ON CONFLICT (dedup_key) DO NOTHING` against the
-- shared UNIQUE index below, with `dedup_key` shaped as
-- `PRESENCE_UPDATE:<guild_id>:<user_id>:<event_time>`.
--
-- Ordering: S3 may land before S1/S4, so every step is re-runnable
-- (`IF NOT EXISTS`): the CREATE covers a missing table and the ALTERs cover
-- a table S1/S4 created first with a narrower column set. Later slices must
-- add their own columns with `ADD COLUMN IF NOT EXISTS` and must reuse the
-- shared dedup index name `idx_event_store_dedup_key` (via
-- `CREATE UNIQUE INDEX IF NOT EXISTS`) rather than adding a second UNIQUE
-- constraint, so the resume-safe key stays a single index.
--
-- Forward-compat nullability: `presence_status`, `activity_type` and
-- `application_id` are NULLABLE so non-presence rows (e.g. S1 message
-- metadata, S2 member deltas, S4 voice rows, which carry no presence enums)
-- can share the table; `guild_id`/`user_id`/`event_time` stay NOT NULL
-- because every slice to date binds them.
--
-- Rollback before merge (table is new and empty): `DROP TABLE IF EXISTS
-- event_store`. After rows accumulate there is no rollback that preserves
-- data; later slices only add columns.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_store (
  id              BIGSERIAL PRIMARY KEY,
  event_type      TEXT NOT NULL,
  guild_id        TEXT NOT NULL,
  user_id         TEXT NOT NULL,
  event_time      timestamptz NOT NULL,
  received_at     timestamptz NOT NULL DEFAULT NOW(),
  dedup_key       TEXT NOT NULL,
  presence_status TEXT,
  activity_type   INTEGER,
  application_id  TEXT
);

ALTER TABLE event_store
  ADD COLUMN IF NOT EXISTS event_type      TEXT,
  ADD COLUMN IF NOT EXISTS guild_id        TEXT,
  ADD COLUMN IF NOT EXISTS user_id         TEXT,
  ADD COLUMN IF NOT EXISTS event_time      timestamptz,
  ADD COLUMN IF NOT EXISTS received_at     timestamptz,
  ADD COLUMN IF NOT EXISTS dedup_key       TEXT,
  ADD COLUMN IF NOT EXISTS presence_status TEXT,
  ADD COLUMN IF NOT EXISTS activity_type   INTEGER,
  ADD COLUMN IF NOT EXISTS application_id  TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_event_store_dedup_key
  ON event_store (dedup_key);
