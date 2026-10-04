-- Voice V3 owner privacy persistence (`/private`, `/public`).
--
-- Mirrors `two_bot_core::voice_private::PrivacyRecord`: whether the room
-- denies Connect to @everyone, the "⇩ Join <owner>" companion voice channel
-- while it does, and the room's block list. Join requests, grants and
-- pending buttons are not persisted yet; they belong to the join-request
-- slice.
--
-- Rooms created before this migration are public with no Join channel.
-- `privacy_touched_at` stamps every privacy write so the rollback delta
-- keeps measuring rooms whose privacy changes in place (NULL = never).
-- A Join channel only exists while the room is private.
--
-- The block list belongs to the room: it survives privacy toggles and
-- ownership changes, and its rows go with the room row (ON DELETE CASCADE),
-- including a room row removed by member erasure.
-- Bot schema range: 0001-0999. Database tests use test containers only.

ALTER TABLE voice_rooms
  ADD COLUMN private BOOLEAN NOT NULL DEFAULT FALSE,
  ADD COLUMN join_channel_id TEXT
    CHECK (join_channel_id IS NULL
      OR (join_channel_id ~ '^[0-9]{1,20}$' AND join_channel_id <> '0')),
  ADD COLUMN privacy_touched_at timestamptz,
  ADD CONSTRAINT voice_rooms_join_channel_requires_private
    CHECK (private OR join_channel_id IS NULL);

CREATE TABLE IF NOT EXISTS voice_room_blocks (
  guild_id          TEXT        NOT NULL,
  room_channel_id   TEXT        NOT NULL,
  blocked_member_id TEXT        NOT NULL
    CHECK (blocked_member_id ~ '^[0-9]{1,20}$' AND blocked_member_id <> '0'),
  created_at        timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (guild_id, room_channel_id, blocked_member_id),
  FOREIGN KEY (guild_id, room_channel_id)
    REFERENCES voice_rooms (guild_id, channel_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_voice_room_blocks_member
  ON voice_room_blocks (guild_id, blocked_member_id);
