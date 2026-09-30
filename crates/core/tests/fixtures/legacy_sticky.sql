-- Sticky/audit subset of legacy two-bot migrations/0015_automations.sql
-- and 0016_automation_claims.sql at b0a26a5e3882dd0784d208079f309893e2ede7e8.
-- Other automation tables are outside this slice. Keep TEXT timestamp types
-- here: the upgrade test must exercise an existing, populated legacy schema.
CREATE TABLE sticky_messages (
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  body TEXT NOT NULL,
  debounce_seconds INTEGER NOT NULL DEFAULT 5,
  enabled BOOLEAN NOT NULL DEFAULT TRUE,
  last_message_id TEXT,
  last_posted_at TEXT,
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_by TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  claim_token TEXT,
  claimed_at TEXT,
  PRIMARY KEY (guild_id, channel_id),
  CONSTRAINT sticky_messages_body CHECK (length(body) BETWEEN 1 AND 2000),
  CONSTRAINT sticky_messages_debounce CHECK (debounce_seconds BETWEEN 1 AND 300)
);

CREATE TABLE automation_audit_log (
  id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  actor_id TEXT,
  action TEXT NOT NULL,
  target_key TEXT,
  outcome TEXT NOT NULL,
  reason TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_automation_audit_guild_time
  ON automation_audit_log (guild_id, created_at);

INSERT INTO sticky_messages VALUES
  ('g-legacy', 'c-posted', 'legacy sticky', 5, TRUE, 'm-legacy',
   '2027-01-15T08:00:00.123Z', 'admin-legacy', '2027-01-15T08:00:00.123Z',
   'admin-legacy', '2027-01-15T09:00:00.123+01:00',
   'legacy-claim', '2027-01-15T08:00:00.123Z'),
  ('g-legacy', 'c-never', 'never posted', 5, TRUE, NULL, NULL,
   'admin-legacy', '2027-01-15T08:00:00.123Z',
   'admin-legacy', '2027-01-15T08:00:00.123Z', NULL, NULL);

INSERT INTO automation_audit_log VALUES
  ('a-legacy', 'g-legacy', 'admin-legacy', 'sticky.create', 'c-posted',
   'ok', NULL, '2027-01-15T09:00:00.123+01:00');
