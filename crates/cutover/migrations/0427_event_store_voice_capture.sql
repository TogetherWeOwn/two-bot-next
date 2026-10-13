-- 0427_event_store_voice_capture: append-only voice capture for event-store S4.
--
-- Creates the append-only `event_store` table (if S1 has not created it yet)
-- and adds the S4 voice columns: identity + session + timestamps
-- (`event_type`, `guild_id`, `user_id`, `channel_id`, `message_id`,
-- `session_id`, `event_time`, `received_at`, `dedup_key`) plus the voice enum
-- flags (`mute`, `deaf`, `self_mute`, `self_deaf`, `self_video`, `self_stream`,
-- `suppress`). Ids, timestamps and enum flags only: no audio columns, no
-- transcript columns, no voice token, no endpoint, no effect text, no status
-- text. Resume-safe dedup is `ON CONFLICT (dedup_key) DO NOTHING` against
-- the shared UNIQUE index below.
--
-- Ordering: S4 may land before S1, so every step is re-runnable
-- (`IF NOT EXISTS`): the CREATE covers a missing table and the ALTERs cover
-- a table S1 created first with a narrower column set. Later slices must add
-- their own columns with `ADD COLUMN IF NOT EXISTS` and must reuse the
-- shared dedup index name `idx_event_store_dedup_key` (via
-- `CREATE UNIQUE INDEX IF NOT EXISTS`) rather than adding a second UNIQUE
-- constraint, so the resume-safe key stays a single index.
--
-- S1 coexistence: S1's acceptance suite pins `message_id` for MESSAGE_CREATE
-- rows, so this migration provisions `message_id TEXT` (NULLABLE) up front.
-- Voice rows leave it NULL; message rows leave the voice flags NULL. The S1
-- exact-column pin (`assert_minimal_schema` equality) stays owned by the S1
-- build card to relax to required-present/forbidden-absent once the union
-- table holds both slices; this slice does not edit the Test Author's S1
-- acceptance file.
--
-- Forward-compat nullability: `channel_id`, `message_id`, `session_id` and
-- every voice flag are NULLABLE so non-voice rows (e.g. S1 message metadata,
-- which has no session) and non-message rows (voice events, which have no
-- message) can share the table; `guild_id`/`user_id` stay NOT NULL
-- because every slice to date binds them.
--
-- Rollback before merge (table is new and empty): `DROP TABLE IF EXISTS
-- event_store`. After rows accumulate there is no rollback that preserves
-- data; later slices only add columns.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_store (
  id          BIGSERIAL PRIMARY KEY,
  event_type  TEXT NOT NULL,
  guild_id    TEXT NOT NULL,
  user_id     TEXT NOT NULL,
  channel_id  TEXT,
  message_id  TEXT,
  session_id  TEXT,
  event_time  timestamptz NOT NULL,
  received_at timestamptz NOT NULL DEFAULT NOW(),
  dedup_key   TEXT NOT NULL,
  mute        BOOLEAN,
  deaf        BOOLEAN,
  self_mute   BOOLEAN,
  self_deaf   BOOLEAN,
  self_video  BOOLEAN,
  self_stream BOOLEAN,
  suppress    BOOLEAN
);

ALTER TABLE event_store
  ADD COLUMN IF NOT EXISTS event_type  TEXT,
  ADD COLUMN IF NOT EXISTS guild_id    TEXT,
  ADD COLUMN IF NOT EXISTS user_id     TEXT,
  ADD COLUMN IF NOT EXISTS channel_id  TEXT,
  ADD COLUMN IF NOT EXISTS message_id  TEXT,
  ADD COLUMN IF NOT EXISTS session_id  TEXT,
  ADD COLUMN IF NOT EXISTS event_time  timestamptz,
  ADD COLUMN IF NOT EXISTS received_at timestamptz,
  ADD COLUMN IF NOT EXISTS dedup_key   TEXT,
  ADD COLUMN IF NOT EXISTS mute        BOOLEAN,
  ADD COLUMN IF NOT EXISTS deaf       BOOLEAN,
  ADD COLUMN IF NOT EXISTS self_mute   BOOLEAN,
  ADD COLUMN IF NOT EXISTS self_deaf   BOOLEAN,
  ADD COLUMN IF NOT EXISTS self_video  BOOLEAN,
  ADD COLUMN IF NOT EXISTS self_stream BOOLEAN,
  ADD COLUMN IF NOT EXISTS suppress    BOOLEAN;

CREATE UNIQUE INDEX IF NOT EXISTS idx_event_store_dedup_key
  ON event_store (dedup_key);
