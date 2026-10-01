-- 0015_automations: admin-defined commands, scheduled messages and stickies
-- (TOG-1648, MEE6 custom commands + StickyBot parity).
--
-- These tables are bot-internal. Nothing here is exposed through web_v1, and
-- no message content is stored: `template`/`body` are admin-authored bot
-- output (the thing the bot is about to say), not member messages. Every
-- mutable definition retains who changed it and when; every execution appends
-- a small audit row without content.
--
-- The SQLite twin of these tables lives in src/store/schema.sql.

CREATE TABLE IF NOT EXISTS automation_commands (
  guild_id      TEXT NOT NULL,
  name          TEXT NOT NULL,
  description   TEXT NOT NULL,
  template      TEXT NOT NULL,
  -- Optional `!trigger` text form. NULL = slash-only. When set, the bot needs
  -- the MessageContent intent to see it, which TWO_TEXT_COMMANDS=1 opts into.
  text_trigger  TEXT,
  enabled       BOOLEAN NOT NULL DEFAULT TRUE,
  created_by    TEXT NOT NULL,
  created_at    TEXT NOT NULL,
  updated_by    TEXT NOT NULL,
  updated_at    TEXT NOT NULL,
  PRIMARY KEY (guild_id, name),
  CONSTRAINT automation_commands_name CHECK (name ~ '^[a-z0-9_-]{1,32}$'),
  CONSTRAINT automation_commands_description CHECK (length(description) BETWEEN 1 AND 100),
  CONSTRAINT automation_commands_template CHECK (length(template) BETWEEN 1 AND 2000),
  CONSTRAINT automation_commands_text_trigger CHECK (
    text_trigger IS NULL OR text_trigger ~ '^![a-z0-9_-]{1,32}$'
  )
);

-- One live text trigger per guild. A slash name and a text trigger may share
-- a word; two text triggers may not, because the lookup is by first word.
CREATE UNIQUE INDEX IF NOT EXISTS idx_automation_commands_trigger
  ON automation_commands (guild_id, lower(text_trigger))
  WHERE text_trigger IS NOT NULL;

CREATE TABLE IF NOT EXISTS scheduled_messages (
  id               TEXT PRIMARY KEY,
  guild_id         TEXT NOT NULL,
  channel_id       TEXT NOT NULL,
  body             TEXT NOT NULL,
  -- ISO-8601 UTC. One-shot when interval_seconds is NULL.
  next_run_at      TEXT NOT NULL,
  interval_seconds INTEGER,
  enabled          BOOLEAN NOT NULL DEFAULT TRUE,
  last_run_at      TEXT,
  last_message_id  TEXT,
  created_by       TEXT NOT NULL,
  created_at       TEXT NOT NULL,
  updated_by       TEXT NOT NULL,
  updated_at       TEXT NOT NULL,
  claim_token      TEXT,
  claimed_at       TEXT,
  CONSTRAINT scheduled_messages_body CHECK (length(body) BETWEEN 1 AND 2000),
  CONSTRAINT scheduled_messages_interval CHECK (
    interval_seconds IS NULL OR interval_seconds BETWEEN 60 AND 31536000
  )
);

-- The scheduler polls this. Partial index: disabled rows never come due.
CREATE INDEX IF NOT EXISTS idx_scheduled_messages_due
  ON scheduled_messages (enabled, next_run_at);

CREATE TABLE IF NOT EXISTS sticky_messages (
  guild_id         TEXT NOT NULL,
  channel_id       TEXT NOT NULL,
  body             TEXT NOT NULL,
  -- Repost only after this many quiet seconds (StickyBot-style debounce).
  debounce_seconds INTEGER NOT NULL DEFAULT 5,
  enabled          BOOLEAN NOT NULL DEFAULT TRUE,
  last_message_id  TEXT,
  last_posted_at   TEXT,
  created_by       TEXT NOT NULL,
  created_at       TEXT NOT NULL,
  updated_by       TEXT NOT NULL,
  updated_at       TEXT NOT NULL,
  claim_token      TEXT,
  claimed_at       TEXT,
  PRIMARY KEY (guild_id, channel_id),
  CONSTRAINT sticky_messages_body CHECK (length(body) BETWEEN 1 AND 2000),
  CONSTRAINT sticky_messages_debounce CHECK (debounce_seconds BETWEEN 1 AND 300)
);

CREATE TABLE IF NOT EXISTS automation_audit_log (
  id         TEXT PRIMARY KEY,
  guild_id   TEXT NOT NULL,
  -- NULL for system actors (the scheduler, the sticky re-poster).
  actor_id   TEXT,
  action     TEXT NOT NULL,
  target_key TEXT,
  outcome    TEXT NOT NULL,
  reason     TEXT,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_automation_audit_guild_time
  ON automation_audit_log (guild_id, created_at);
