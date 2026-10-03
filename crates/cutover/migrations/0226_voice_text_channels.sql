-- V9b companion text-channel persistence (TOG-12802).
--
-- Per-creator `/textchannels` settings ride the `voice_creators` row:
-- `text_channel_name` is the configured companion name (NULL = the default
-- companion name at plan time); `text_viewer_role_id` is the one extra role
-- allowed to view (NULL = occupants and admins only, the guild id =
-- @everyone). The toggle itself stays the existing `text_channels` column,
-- off by default. Bounds mirror `CreatorChannel::validate`: a configured
-- name is non-blank and at most 100 characters, a viewer role is a nonzero
-- snowflake.
--
-- Each created companion is recorded in `voice_text_companions` with the
-- settings snapshot taken at creation. Later settings changes affect only
-- channels created afterwards, so the snapshot is never updated in place;
-- the row is deleted with its room.
--
-- Deliberately no foreign key to `voice_rooms` (same choice as 0224): the
-- room row is deleted first when the room empties and the companion delete
-- follows from the same worker, so a key would couple the two writes'
-- ordering without adding safety. A room deleted by hand leaves its
-- companion row for reconciliation instead of cascading.
-- Bot schema range: 0001-0999. Database tests use test containers only.

ALTER TABLE voice_creators
  ADD COLUMN text_channel_name TEXT
    CHECK (text_channel_name IS NULL
      OR char_length(btrim(text_channel_name)) BETWEEN 1 AND 100),
  ADD COLUMN text_viewer_role_id TEXT
    CHECK (text_viewer_role_id IS NULL
      OR (text_viewer_role_id ~ '^[0-9]{1,20}$' AND text_viewer_role_id <> '0'));

CREATE TABLE IF NOT EXISTS voice_text_companions (
  guild_id            TEXT        NOT NULL,
  room_channel_id     TEXT        NOT NULL,
  text_channel_id     TEXT        NOT NULL,
  text_channels       BOOLEAN     NOT NULL,
  text_channel_name   TEXT
    CHECK (text_channel_name IS NULL
      OR char_length(btrim(text_channel_name)) BETWEEN 1 AND 100),
  text_viewer_role_id TEXT
    CHECK (text_viewer_role_id IS NULL
      OR (text_viewer_role_id ~ '^[0-9]{1,20}$' AND text_viewer_role_id <> '0')),
  created_at          timestamptz NOT NULL,
  PRIMARY KEY (guild_id, room_channel_id)
);
