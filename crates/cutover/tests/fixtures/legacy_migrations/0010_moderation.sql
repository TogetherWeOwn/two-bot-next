-- 0010_moderation: durable warnings, scheduled tempban expiry, and one audit row
-- per moderation action. Request bodies are not stored; the reason is the
-- moderator-supplied audit reason and metadata contains only bounded numbers.

CREATE TABLE IF NOT EXISTS moderation_warnings (
  id         TEXT PRIMARY KEY,
  guild_id   TEXT NOT NULL,
  user_id    TEXT NOT NULL,
  actor_id   TEXT NOT NULL,
  reason     TEXT NOT NULL,
  request_id TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_moderation_warnings_user
  ON moderation_warnings (guild_id, user_id, created_at);

CREATE TABLE IF NOT EXISTS moderation_scheduled_unbans (
  request_id   TEXT PRIMARY KEY,
  guild_id     TEXT NOT NULL,
  user_id      TEXT NOT NULL,
  execute_at   TEXT NOT NULL,
  reason       TEXT NOT NULL,
  state        TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  completed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_moderation_unbans_due
  ON moderation_scheduled_unbans (state, execute_at);

CREATE TABLE IF NOT EXISTS moderation_audit (
  request_id      TEXT PRIMARY KEY,
  guild_id        TEXT NOT NULL,
  actor_id        TEXT NOT NULL,
  action          TEXT NOT NULL,
  target_id       TEXT,
  channel_id      TEXT,
  reason          TEXT NOT NULL,
  outcome         TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  metadata_json   TEXT NOT NULL,
  created_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_moderation_audit_time
  ON moderation_audit (guild_id, created_at);
CREATE INDEX IF NOT EXISTS idx_moderation_audit_target
  ON moderation_audit (guild_id, target_id, created_at);
