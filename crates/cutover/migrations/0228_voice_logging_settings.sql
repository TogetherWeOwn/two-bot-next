-- V10a guild voice-room logging settings (TOG-10111).
--
-- One row per guild, written whole by `PgRoomStore::save_logging_settings`.
-- No row means the defaults: brief notices, no configured channel, no
-- mention. Notice routing then follows the spec's fallback chain.
--
-- `detail_level` is the `/logging` level. `log_channel_id` is an optional
-- nonzero channel snowflake tried before the fallback chain.
-- `mention_role_id` is an optional nonzero role snowflake mentioned on error
-- notices; only a role is stored so the row holds no member IDs.
-- Snowflakes are strings because values above 2^53 do not survive JSON numbers.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS voice_logging_settings (
  guild_id        TEXT PRIMARY KEY,
  detail_level    TEXT NOT NULL DEFAULT 'brief'
    CHECK (detail_level IN ('off', 'brief', 'full')),
  log_channel_id  TEXT
    CHECK (log_channel_id IS NULL
      OR (log_channel_id ~ '^[0-9]{1,20}$' AND log_channel_id <> '0')),
  mention_role_id TEXT
    CHECK (mention_role_id IS NULL
      OR (mention_role_id ~ '^[0-9]{1,20}$' AND mention_role_id <> '0'))
);
