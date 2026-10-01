-- 0013_automod: durable violation count used by the sanctions ladder.
-- Message content and matched excerpts are never stored.

CREATE TABLE IF NOT EXISTS automod_violations (
  guild_id         TEXT NOT NULL,
  user_id          TEXT NOT NULL,
  violation_count  INTEGER NOT NULL,
  last_filter      TEXT NOT NULL,
  last_message_id  TEXT NOT NULL,
  updated_at       TEXT NOT NULL,
  PRIMARY KEY (guild_id, user_id),
  UNIQUE (guild_id, last_message_id)
);

CREATE INDEX IF NOT EXISTS idx_automod_violations_updated
  ON automod_violations (guild_id, updated_at);
