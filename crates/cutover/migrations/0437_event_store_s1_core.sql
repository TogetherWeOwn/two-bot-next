-- 0437_event_store_s1_core: append-only core + MESSAGE_CREATE metadata for
-- event-store S1 (parent scope TOG-22849 slice 1).
--
-- Creates the append-only `event_store` table (if no sibling slice created it
-- yet) with the S1 core columns: identity + timestamps (`event_type`,
-- `guild_id`, `channel_id`, `message_id`, `user_id`, `event_time`,
-- `received_at`, `dedup_key`). Ids, timestamps and the event-type enum only:
-- no content/body column, no embeds column, no attachments column, no
-- username column, no custom-status text column, no raw-payload json/jsonb
-- column (see the FORBIDDEN_COLUMNS plus json/jsonb fence in
-- crates/discord/tests/event_store_s1.rs). Resume-safe dedup is
-- `ON CONFLICT (dedup_key) DO NOTHING` against the shared UNIQUE index
-- below, with `dedup_key` shaped as `MESSAGE_CREATE:<message_id>`; same-key
-- redelivery inserts nothing and the first row wins on replay.
--
-- Ordering: S1 may land before or after any sibling slice, so every step is
-- re-runnable. The CREATE covers a missing table and the ALTERs cover a
-- table a sibling created first with a wider column set; `ADD COLUMN IF
-- NOT EXISTS` never changes an existing column, so the explicit
-- `DROP NOT NULL` relaxations below converge the shared nullable columns no
-- matter which slice created the table first. Later slices must add their
-- own columns with `ADD COLUMN IF NOT EXISTS` and must reuse the shared
-- dedup index name `idx_event_store_dedup_key` (via
-- `CREATE UNIQUE INDEX IF NOT EXISTS`) rather than adding a second UNIQUE
-- constraint, so the resume-safe key stays a single index.
--
-- Forward-compat nullability: `channel_id`, `message_id` and `user_id` are
-- NULLABLE so non-message rows (e.g. presence rows, voice rows, thread
-- lifecycle rows, stage/interaction/audit rows, which carry no message or
-- member ids) can share the table. `guild_id`/`event_time` stay NOT NULL
-- because every slice to date binds them. No snowflake CHECK constraints on
-- guild/channel/message/user ids: fixtures and early guilds carry short
-- numeric ids, and a format check would refuse them.
--
-- Erasure: rows are keyed by guild_id + channel_id + message_id (+ user_id
-- for the author) with no other identity, so per-guild purge and per-message
-- erasure are plain DELETEs on those columns; retention windows stay with
-- the retention slice, not this migration. Roles/seeds: none; the table
-- inherits the database's default grants and carries no seed rows.
--
-- Rollback before merge (table is new and empty): `DROP TABLE IF EXISTS
-- event_store`. After rows accumulate there is no rollback that preserves
-- data; later slices only add columns.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_store (
  id          BIGSERIAL PRIMARY KEY,
  event_type  TEXT NOT NULL,
  guild_id    TEXT NOT NULL,
  channel_id  TEXT,
  message_id  TEXT,
  user_id     TEXT,
  event_time  timestamptz NOT NULL,
  received_at timestamptz NOT NULL DEFAULT NOW(),
  dedup_key   TEXT NOT NULL
);

ALTER TABLE event_store
  ADD COLUMN IF NOT EXISTS event_type  TEXT,
  ADD COLUMN IF NOT EXISTS guild_id    TEXT,
  ADD COLUMN IF NOT EXISTS channel_id  TEXT,
  ADD COLUMN IF NOT EXISTS message_id  TEXT,
  ADD COLUMN IF NOT EXISTS user_id     TEXT,
  ADD COLUMN IF NOT EXISTS event_time  timestamptz,
  ADD COLUMN IF NOT EXISTS received_at timestamptz,
  ADD COLUMN IF NOT EXISTS dedup_key   TEXT;

-- Non-message sibling rows carry no channel/message/member ids: relax these
-- columns when an earlier slice created the table first with one of them
-- NOT NULL. Safe when already nullable (no-op) and ordered after the ADD
-- COLUMN above so each column always exists here.
ALTER TABLE event_store ALTER COLUMN channel_id DROP NOT NULL;
ALTER TABLE event_store ALTER COLUMN message_id DROP NOT NULL;
ALTER TABLE event_store ALTER COLUMN user_id DROP NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_event_store_dedup_key
  ON event_store (dedup_key);
