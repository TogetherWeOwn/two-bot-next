-- V10b guild-level voice-room controls (TOG-10111).
--
-- One row per guild, written whole by `PgRoomStore::save_access_controls`
-- after `validate_access_controls`. No row means the defaults: room creation
-- on, no required role, no per-command restrictions.
--
-- `required_role_id` is a nonzero snowflake or NULL (no guild-wide gate).
-- `command_roles` maps a voice command name to the role IDs allowed to use
-- it, as a JSON object of string arrays. A present key with an empty array
-- denies every non-admin (fail closed); removing the key lifts the
-- restriction. Role IDs are strings because snowflakes above 2^53 do not
-- survive JSON numbers. Unknown command names are refused in code, so the
-- column only needs to guarantee the shape.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS voice_access_controls (
  guild_id              TEXT    PRIMARY KEY,
  room_creation_enabled BOOLEAN NOT NULL DEFAULT TRUE,
  required_role_id      TEXT
    CHECK (required_role_id IS NULL
      OR (required_role_id ~ '^[0-9]{1,20}$' AND required_role_id <> '0')),
  command_roles         JSONB   NOT NULL DEFAULT '{}'::jsonb
    CHECK (jsonb_typeof(command_roles) = 'object')
);
