-- 0014_automod_idempotency: remember every sanctioned message, not only the
-- latest message for a member. This prevents delayed gateway retries from
-- advancing the sanctions ladder twice.

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
SELECT guild_id, last_message_id, user_id, updated_at
  FROM automod_violations
ON CONFLICT (guild_id, message_id) DO NOTHING;
