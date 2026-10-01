-- Temporary voice rooms V1 foundation (TOG-10093).
--
-- Mirrors `two_bot_core::voice_rooms::{CreatorChannel, VoiceRoom}`:
-- snowflakes as TEXT (same convention as the cutover migrations), room
-- settings bounded by the same CHECKs as `CreatorChannel::validate`.
--
-- Deliberately no foreign key from `voice_rooms` to `voice_creators`: rooms
-- outlive their creator config (unmarking a creator never orphans live
-- rooms; reconciliation forgets only channels that no longer exist).
-- Kept in the existing shared migration history so runtime and cutover do
-- not install competing version-0001 checksums in `_sqlx_migrations`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS voice_creators (
  guild_id              TEXT    NOT NULL,
  channel_id            TEXT    NOT NULL,
  name_template         TEXT    NOT NULL DEFAULT '',
  permission_source     TEXT    NOT NULL DEFAULT 'creator'
    CHECK (permission_source IN ('creator', 'category', 'channel')),
  permission_channel_id TEXT,
  default_limit         INTEGER NOT NULL DEFAULT 0
    CHECK (default_limit BETWEEN 0 AND 99),
  private_default       BOOLEAN NOT NULL DEFAULT FALSE,
  text_channels         BOOLEAN NOT NULL DEFAULT FALSE,
  position              TEXT    NOT NULL DEFAULT 'above'
    CHECK (position IN ('above', 'below')),
  first_room_number     BIGINT  NOT NULL DEFAULT 1
    CHECK (first_room_number >= 1),
  PRIMARY KEY (guild_id, channel_id),
  CHECK (permission_source <> 'channel' OR permission_channel_id IS NOT NULL)
);

CREATE TABLE IF NOT EXISTS voice_rooms (
  guild_id           TEXT        NOT NULL,
  channel_id         TEXT        NOT NULL,
  creator_channel_id TEXT        NOT NULL,
  owner_id           TEXT        NOT NULL,
  original_creator_id TEXT       NOT NULL,
  -- u64 seed; TEXT because values above 2^63-1 do not fit BIGINT.
  name_seed          TEXT        NOT NULL,
  created_at         timestamptz NOT NULL,
  PRIMARY KEY (guild_id, channel_id)
);

CREATE INDEX IF NOT EXISTS idx_voice_rooms_owner
  ON voice_rooms (guild_id, owner_id);
