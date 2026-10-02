-- Evidence schema only; does not activate containment or automatic removal.
-- Legacy 0015_anti_nuke_containment columns, preserving TEXT ISO timestamps.
CREATE TABLE IF NOT EXISTS join_risk_flags (
  event_id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  member_id TEXT NOT NULL,
  account_created_at TEXT NOT NULL,
  joined_at TEXT NOT NULL,
  source TEXT NOT NULL,
  score INTEGER NOT NULL CHECK (score >= 0),
  reasons_json TEXT NOT NULL,
  bulk_join_window BOOLEAN NOT NULL,
  flagged BOOLEAN NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_join_risk_flags_time ON join_risk_flags (guild_id, joined_at);
