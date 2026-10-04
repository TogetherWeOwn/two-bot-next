-- Custom commands + shared automation audit (TOG-10080, block 0130-0139).
--
-- Port of legacy two-bot `migrations/0015_automations.sql`, command columns
-- only: sibling slices own the rest (TOG-10081 scheduled_messages, TOG-10082
-- sticky_messages). Keeps legacy table/column names. Timestamps are
-- timestamptz (the port writes ISO-8601 UTC, which Postgres parses exactly);
-- the port generates audit ids client-side as UUID text, so no pgcrypto is
-- required. Schedules/stickies write to the same `automation_audit_log`
-- with their own action names; nothing here assumes which slice wrote a row.
--
-- `CREATE TABLE IF NOT EXISTS` throughout: the tools run against staging and
-- agent-testdb scratch schemas that may already carry S6-ported tables, and
-- a migration must never fail a re-run on DDL.

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
  created_at    timestamptz NOT NULL,
  updated_by    TEXT NOT NULL,
  updated_at    timestamptz NOT NULL,
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

-- Shared by all automation slices: ids and outcomes only, never message
-- content. `actor_id` is NULL for system actors. `reason` carries a short
-- stable code (e.g. `reserved_name`), never secrets.
CREATE TABLE IF NOT EXISTS automation_audit_log (
  id         TEXT PRIMARY KEY,
  guild_id   TEXT NOT NULL,
  actor_id   TEXT,
  action     TEXT NOT NULL,
  target_key TEXT,
  outcome    TEXT NOT NULL,
  reason     TEXT,
  created_at timestamptz NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_automation_audit_guild_time
  ON automation_audit_log (guild_id, created_at);
