-- MEE6-compatible leveling state (TOG-1645).
--
-- member_levels is the current projection; xp_awards and level_import_runs are
-- the audit trail. Imported XP is kept separate from organic message/voice XP
-- so a corrected export can replace the import without erasing activity earned
-- after migration.
CREATE TABLE member_levels (
  guild_id    TEXT        NOT NULL,
  member_id   TEXT        NOT NULL,
  xp          BIGINT      NOT NULL CHECK (xp >= 0),
  message_xp  BIGINT      NOT NULL DEFAULT 0 CHECK (message_xp >= 0),
  voice_xp    BIGINT      NOT NULL DEFAULT 0 CHECK (voice_xp >= 0),
  imported_xp BIGINT      NOT NULL DEFAULT 0 CHECK (imported_xp >= 0),
  updated_at  timestamptz NOT NULL,
  PRIMARY KEY (guild_id, member_id),
  CHECK (xp = message_xp + voice_xp + imported_xp)
);

CREATE INDEX idx_member_levels_rank
  ON member_levels (guild_id, xp DESC, member_id ASC);

CREATE TABLE xp_cooldowns (
  guild_id       TEXT        NOT NULL,
  member_id      TEXT        NOT NULL,
  source         TEXT        NOT NULL CHECK (source IN ('message', 'voice')),
  last_awarded_at timestamptz NOT NULL,
  PRIMARY KEY (guild_id, member_id, source)
);

CREATE TABLE xp_awards (
  id          BIGSERIAL   PRIMARY KEY,
  guild_id    TEXT        NOT NULL,
  member_id   TEXT        NOT NULL,
  source      TEXT        NOT NULL CHECK (source IN ('message', 'voice')),
  xp          INTEGER     NOT NULL CHECK (xp > 0),
  occurred_at timestamptz NOT NULL,
  channel_id  TEXT
);

CREATE INDEX idx_xp_awards_member_time
  ON xp_awards (guild_id, member_id, occurred_at DESC);

CREATE TABLE level_role_rewards (
  guild_id TEXT    NOT NULL,
  level    INTEGER NOT NULL CHECK (level > 0),
  role_id  TEXT    NOT NULL,
  PRIMARY KEY (guild_id, level),
  UNIQUE (guild_id, role_id)
);

CREATE TABLE level_import_runs (
  id                BIGSERIAL   PRIMARY KEY,
  guild_id          TEXT        NOT NULL,
  source            TEXT        NOT NULL CHECK (source = 'mee6'),
  source_rows       INTEGER     NOT NULL,
  unique_members    INTEGER     NOT NULL,
  inserted          INTEGER     NOT NULL,
  updated           INTEGER     NOT NULL,
  unchanged         INTEGER     NOT NULL,
  duplicate_rows    INTEGER     NOT NULL,
  total_imported_xp BIGINT      NOT NULL,
  imported_at       timestamptz NOT NULL
);
