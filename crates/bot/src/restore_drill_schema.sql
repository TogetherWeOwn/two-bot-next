-- Scratch-only archive compatibility, applied AFTER shipped S6 migrations only
-- to the newly allocated restore-drill DB. Not a production migration or a
-- feature initializer: no consumers, roles, grants, backfill or activation.
-- Exact legacy definitions preserved in cutover/tests/fixtures/legacy_migrations:
-- 0013_automod, 0014_automod_idempotency, 0015_anti_nuke_containment,
-- 0015_automations and 0017_scheduled_occurrence_nonce. Pin them in the offline
-- regression. IF NOT EXISTS leaves a future S6-owned definition authoritative.

CREATE TABLE IF NOT EXISTS containment_events (
  audit_entry_id TEXT PRIMARY KEY,
  guild_id       TEXT NOT NULL,
  executor_id    TEXT,
  action         TEXT NOT NULL,
  target_id      TEXT,
  weight         INTEGER NOT NULL CHECK (weight > 0),
  occurred_at    TEXT NOT NULL,
  state          TEXT NOT NULL CHECK (state IN ('observe', 'contain', 'ignored', 'stale')),
  reason         TEXT NOT NULL,
  created_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_containment_events_heat
  ON containment_events (guild_id, executor_id, occurred_at);

CREATE TABLE IF NOT EXISTS containment_incidents (
  id                     TEXT PRIMARY KEY,
  guild_id               TEXT NOT NULL,
  executor_id            TEXT NOT NULL,
  trigger_audit_entry_id TEXT NOT NULL UNIQUE REFERENCES containment_events(audit_entry_id),
  heat                    INTEGER NOT NULL CHECK (heat > 0),
  state                   TEXT NOT NULL CHECK (state IN ('containing', 'contained', 'dry_run', 'refused', 'uncertain', 'failed')),
  result_json             TEXT,
  started_at              TEXT NOT NULL,
  cooldown_until          TEXT,
  completed_at            TEXT
);
CREATE INDEX IF NOT EXISTS idx_containment_incident_executor
  ON containment_incidents (guild_id, executor_id, started_at);

CREATE TABLE IF NOT EXISTS join_risk_flags (
  event_id           TEXT PRIMARY KEY,
  guild_id           TEXT NOT NULL,
  member_id          TEXT NOT NULL,
  account_created_at TEXT NOT NULL,
  joined_at          TEXT NOT NULL,
  source             TEXT NOT NULL,
  score              INTEGER NOT NULL CHECK (score >= 0),
  reasons_json       TEXT NOT NULL,
  bulk_join_window   BOOLEAN NOT NULL,
  flagged            BOOLEAN NOT NULL,
  created_at         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_join_risk_flags_time
  ON join_risk_flags (guild_id, joined_at);

CREATE TABLE IF NOT EXISTS automation_commands (
  guild_id      TEXT NOT NULL,
  name          TEXT NOT NULL,
  description   TEXT NOT NULL,
  template      TEXT NOT NULL,
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
CREATE UNIQUE INDEX IF NOT EXISTS idx_automation_commands_trigger
  ON automation_commands (guild_id, lower(text_trigger))
  WHERE text_trigger IS NOT NULL;

CREATE TABLE IF NOT EXISTS scheduled_messages (
  id               TEXT PRIMARY KEY,
  guild_id         TEXT NOT NULL,
  channel_id       TEXT NOT NULL,
  body             TEXT NOT NULL,
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
CREATE INDEX IF NOT EXISTS idx_scheduled_messages_due
  ON scheduled_messages (enabled, next_run_at);
ALTER TABLE scheduled_messages ADD COLUMN IF NOT EXISTS occurrence_nonce TEXT;

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

CREATE TABLE IF NOT EXISTS automod_processed_messages (
  guild_id     TEXT NOT NULL,
  message_id   TEXT NOT NULL,
  user_id      TEXT NOT NULL,
  processed_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, message_id)
);
CREATE INDEX IF NOT EXISTS idx_automod_processed_user
  ON automod_processed_messages (guild_id, user_id, processed_at);
