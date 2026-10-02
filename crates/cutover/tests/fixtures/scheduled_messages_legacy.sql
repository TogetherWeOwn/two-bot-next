-- Legacy queue before claim fencing and occurrence nonces.
CREATE TABLE scheduled_messages (
  id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  body TEXT NOT NULL,
  next_run_at TEXT NOT NULL,
  interval_seconds INTEGER,
  enabled BOOLEAN NOT NULL DEFAULT TRUE,
  last_run_at TEXT,
  last_message_id TEXT,
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_by TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  CONSTRAINT scheduled_messages_body CHECK (length(body) BETWEEN 1 AND 2000),
  CONSTRAINT scheduled_messages_interval CHECK (
    interval_seconds IS NULL OR interval_seconds BETWEEN 60 AND 31536000
  )
);

INSERT INTO scheduled_messages
  (id, guild_id, channel_id, body, next_run_at, interval_seconds,
   last_run_at, last_message_id, created_by, created_at, updated_by, updated_at)
VALUES
  ('legacy-once', 'sched-legacy', 'chan9', 'one-shot body', '2026-01-01T00:00:00.000Z', NULL,
   NULL, NULL, 'creator', '2025-12-31T00:00:00.000Z', 'editor', '2025-12-31T12:00:00.000Z'),
  ('legacy-hourly', 'sched-legacy', 'chan9', 'recurring body', '2026-01-01T01:00:00.000Z', 3600,
   '2026-01-01T00:00:00.000Z', 'msg-before-upgrade', 'creator', '2025-12-31T00:00:00.000Z', 'editor', '2025-12-31T12:00:00.000Z');
