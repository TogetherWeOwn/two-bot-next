-- V11b configuration persistence (TOG-12875).
--
-- Persists `two_bot_core::voice_config::VoiceConfiguration` for `/export` ->
-- `/import`: adds `status_template` and `group_by_category` to
-- `voice_creators`, plus new guild-scoped tables for channel templates, game
-- aliases, random lists (+ choices), logging config (+ mention members/roles)
-- and guild settings (+ command roles). Bounds mirror
-- `voice_config::validate_configuration`: snowflakes are canonical nonzero
-- decimal u64 strings (format CHECK; u64 range is application-checked),
-- limits 0..=99, first numbers >= 1, free-text keys non-blank, literal names
-- 1..100 non-blank characters, time zones non-blank, logging detail in
-- ('errors', 'lifecycle', 'verbose').
--
-- Deliberately no foreign keys (same choice as 0224/0226): `apply` replaces
-- all sections in one transaction with explicit DELETEs, so keys would couple
-- write ordering without adding safety. Configuration never touches
-- `voice_rooms` or companions.
-- Bot schema range: 0001-0999. Database tests use test containers only.

ALTER TABLE voice_creators
  ADD COLUMN status_template TEXT,
  ADD COLUMN group_by_category BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE IF NOT EXISTS voice_channel_templates (
  guild_id        TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  channel_id      TEXT NOT NULL
    CHECK (channel_id ~ '^[0-9]{1,20}$' AND channel_id <> '0' AND channel_id NOT LIKE '0%'),
  name_template   TEXT NOT NULL,
  status_template TEXT,
  PRIMARY KEY (guild_id, channel_id)
);

CREATE TABLE IF NOT EXISTS voice_game_aliases (
  guild_id TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  game       TEXT NOT NULL
    CHECK (char_length(btrim(game)) >= 1),
  alias      TEXT NOT NULL
    CHECK (char_length(btrim(alias)) >= 1),
  PRIMARY KEY (guild_id, game)
);

CREATE TABLE IF NOT EXISTS voice_random_lists (
  guild_id TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  name       TEXT NOT NULL
    CHECK (char_length(btrim(name)) >= 1),
  PRIMARY KEY (guild_id, name)
);

CREATE TABLE IF NOT EXISTS voice_random_list_choices (
  guild_id  TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  list_name TEXT NOT NULL
    CHECK (char_length(btrim(list_name)) >= 1),
  position  INTEGER NOT NULL CHECK (position >= 0),
  choice    TEXT NOT NULL
    CHECK (char_length(btrim(choice)) >= 1),
  PRIMARY KEY (guild_id, list_name, position)
);

CREATE TABLE IF NOT EXISTS voice_logging (
  guild_id   TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  channel_id TEXT NOT NULL
    CHECK (channel_id ~ '^[0-9]{1,20}$' AND channel_id <> '0' AND channel_id NOT LIKE '0%'),
  detail     TEXT NOT NULL CHECK (detail IN ('errors', 'lifecycle', 'verbose')),
  PRIMARY KEY (guild_id)
);

CREATE TABLE IF NOT EXISTS voice_logging_mention_members (
  guild_id  TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  member_id TEXT NOT NULL
    CHECK (member_id ~ '^[0-9]{1,20}$' AND member_id <> '0' AND member_id NOT LIKE '0%'),
  PRIMARY KEY (guild_id, member_id)
);

CREATE TABLE IF NOT EXISTS voice_logging_mention_roles (
  guild_id TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  role_id  TEXT NOT NULL
    CHECK (role_id ~ '^[0-9]{1,20}$' AND role_id <> '0' AND role_id NOT LIKE '0%'),
  PRIMARY KEY (guild_id, role_id)
);

CREATE TABLE IF NOT EXISTS voice_guild_settings (
  guild_id                       TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  creation_enabled               BOOLEAN NOT NULL,
  unique_names                   BOOLEAN NOT NULL,
  no_game_label                  TEXT NOT NULL
    CHECK (char_length(btrim(no_game_label)) BETWEEN 1 AND 100),
  force_single_game              BOOLEAN NOT NULL,
  count_members_without_activity BOOLEAN NOT NULL,
  time_zone                      TEXT NOT NULL
    CHECK (char_length(btrim(time_zone)) >= 1),
  text_channel_name              TEXT NOT NULL
    CHECK (char_length(btrim(text_channel_name)) BETWEEN 1 AND 100),
  text_viewer_role_id            TEXT
    CHECK (text_viewer_role_id IS NULL
      OR (text_viewer_role_id ~ '^[0-9]{1,20}$'
        AND text_viewer_role_id <> '0'
        AND text_viewer_role_id NOT LIKE '0%')),
  command_role_id                TEXT
    CHECK (command_role_id IS NULL
      OR (command_role_id ~ '^[0-9]{1,20}$'
        AND command_role_id <> '0'
        AND command_role_id NOT LIKE '0%')),
  PRIMARY KEY (guild_id)
);

CREATE TABLE IF NOT EXISTS voice_command_roles (
  guild_id TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  command  TEXT NOT NULL
    CHECK (char_length(btrim(command)) >= 1),
  PRIMARY KEY (guild_id, command)
);

CREATE TABLE IF NOT EXISTS voice_command_role_members (
  guild_id TEXT NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  command  TEXT NOT NULL
    CHECK (char_length(btrim(command)) >= 1),
  role_id  TEXT NOT NULL
    CHECK (role_id ~ '^[0-9]{1,20}$' AND role_id <> '0' AND role_id NOT LIKE '0%'),
  PRIMARY KEY (guild_id, command, role_id)
);
