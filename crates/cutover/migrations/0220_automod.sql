-- Legacy 0013/0014 automod ledger. No content or matched excerpts are stored.
CREATE TABLE IF NOT EXISTS automod_violations (
  guild_id         TEXT NOT NULL,
  user_id          TEXT NOT NULL,
  violation_count  INTEGER NOT NULL CHECK (violation_count > 0),
  last_filter      TEXT NOT NULL,
  last_message_id  TEXT NOT NULL,
  updated_at       TEXT NOT NULL,
  PRIMARY KEY (guild_id, user_id),
  UNIQUE (guild_id, last_message_id)
);

CREATE INDEX IF NOT EXISTS idx_automod_violations_updated
  ON automod_violations (guild_id, updated_at);

CREATE TABLE IF NOT EXISTS automod_processed_messages (
  guild_id     TEXT NOT NULL,
  message_id   TEXT NOT NULL,
  user_id      TEXT NOT NULL,
  processed_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_automod_processed_user
  ON automod_processed_messages (guild_id, user_id, processed_at);

INSERT INTO automod_processed_messages (guild_id, message_id, user_id, processed_at)
SELECT guild_id, last_message_id, user_id, updated_at FROM automod_violations
ON CONFLICT (guild_id, message_id) DO NOTHING;
