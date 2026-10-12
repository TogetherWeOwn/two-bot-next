-- 0429_event_store_thread_capture: append-only thread lifecycle + member-delta
-- capture for event-store S6 (parent scope TOG-22849 slice 6).
--
-- Creates the append-only `event_store` table (if S1 has not created it yet)
-- and adds the S6 thread columns: identity + timestamps (`event_type`,
-- `guild_id`, `thread_id`, `parent_id`, `event_time`, `received_at`,
-- `dedup_key`) plus the thread-state enums (`archived`, `locked`,
-- `invitable`) and the member-id delta (`user_id`). Ids, timestamps and
-- enum fields only: no thread-name column, no topic column, no message-body
-- column, no username column (see the FORBIDDEN_COLUMNS tripwire in
-- crates/discord/tests/event_store_s6.rs). Resume-safe dedup is
-- `ON CONFLICT (dedup_key) DO NOTHING` against the shared UNIQUE index
-- below, with `dedup_key` shaped per event as
-- `<EVENT_TYPE>:<guild_id>:<thread_id>:<event_time>` (member deltas append
-- `:<user_id>`) and THREAD_LIST_SYNC fanning out one row per listed thread
-- under a shared `THREAD_LIST_SYNC:<guild>:<sync_time>` prefix.
--
-- Ordering: S6 may land before S1, so every step is re-runnable
-- (`IF NOT EXISTS`): the CREATE covers a missing table and the ALTERs cover
-- a table S1/S2/S3/S4 created first with a narrower column set. Later slices
-- must add their own columns with `ADD COLUMN IF NOT EXISTS` and must reuse
-- the shared dedup index name `idx_event_store_dedup_key` (via
-- `CREATE UNIQUE INDEX IF NOT EXISTS`) rather than adding a second UNIQUE
-- constraint, so the resume-safe key stays a single index.
--
-- Forward-compat nullability: `thread_id`, `parent_id`, `user_id`,
-- `archived`, `locked` and `invitable` are NULLABLE so non-thread rows (e.g.
-- S1 message metadata, S2 member rows, S3 presence rows, S4 voice rows,
-- which carry no thread ids or flags) can share the table, and so thread
-- lifecycle rows (which carry no member id) and THREAD_DELETE rows (which
-- carry no state enums) insert cleanly. `guild_id`/`event_time` stay NOT
-- NULL because every slice to date binds them. `user_id` is created
-- NULLABLE here, and the explicit `DROP NOT NULL` below relaxes it when an
-- earlier slice (S2/S4) created the table first with `user_id NOT NULL`;
-- without that step their NOT NULL would refuse S6 thread lifecycle rows.
--
-- Erasure: rows are keyed by guild_id + thread_id (+ user_id for member
-- deltas) with no other identity, so per-guild purge and per-user erasure
-- are plain DELETEs on those columns; retention windows stay with the
-- retention slice, not this migration.
-- Roles/seeds: none; the table inherits the database's default grants and
-- carries no seed rows.
-- Rollback: `ALTER TABLE event_store DROP COLUMN IF EXISTS thread_id`,
-- `DROP COLUMN IF EXISTS parent_id`, `DROP COLUMN IF EXISTS archived`,
-- `DROP COLUMN IF EXISTS locked`, `DROP COLUMN IF EXISTS invitable`
-- (plus the table itself via `DROP TABLE IF EXISTS event_store` solely when
-- this migration created the table and nothing else uses it).
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_store (
  id          BIGSERIAL PRIMARY KEY,
  event_type  TEXT NOT NULL,
  guild_id    TEXT NOT NULL,
  thread_id   TEXT,
  parent_id   TEXT,
  user_id     TEXT,
  event_time  timestamptz NOT NULL,
  received_at timestamptz NOT NULL DEFAULT NOW(),
  dedup_key   TEXT NOT NULL,
  archived    BOOLEAN,
  locked      BOOLEAN,
  invitable   BOOLEAN
);

ALTER TABLE event_store
  ADD COLUMN IF NOT EXISTS event_type  TEXT,
  ADD COLUMN IF NOT EXISTS guild_id    TEXT,
  ADD COLUMN IF NOT EXISTS thread_id   TEXT,
  ADD COLUMN IF NOT EXISTS parent_id   TEXT,
  ADD COLUMN IF NOT EXISTS user_id     TEXT,
  ADD COLUMN IF NOT EXISTS event_time  timestamptz,
  ADD COLUMN IF NOT EXISTS received_at timestamptz,
  ADD COLUMN IF NOT EXISTS dedup_key   TEXT,
  ADD COLUMN IF NOT EXISTS archived    BOOLEAN,
  ADD COLUMN IF NOT EXISTS locked      BOOLEAN,
  ADD COLUMN IF NOT EXISTS invitable   BOOLEAN;

-- Thread lifecycle rows carry no member id; relax user_id when an earlier
-- slice created it NOT NULL. Safe when already nullable (no-op) and ordered
-- after the ADD COLUMN above so the column always exists here.
ALTER TABLE event_store ALTER COLUMN user_id DROP NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_event_store_dedup_key
  ON event_store (dedup_key);
