-- Durable claims only; does not register an audit listener or arm containment.
-- Legacy 0015_anti_nuke_containment columns, preserving TEXT ISO timestamps.
CREATE TABLE IF NOT EXISTS containment_events (
  audit_entry_id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  executor_id TEXT,
  action TEXT NOT NULL,
  target_id TEXT,
  weight INTEGER NOT NULL CHECK (weight > 0),
  occurred_at TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('observe', 'contain', 'ignored', 'stale')),
  reason TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_containment_events_heat
  ON containment_events (guild_id, executor_id, occurred_at);

CREATE TABLE IF NOT EXISTS containment_incidents (
  id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  executor_id TEXT NOT NULL,
  trigger_audit_entry_id TEXT NOT NULL UNIQUE REFERENCES containment_events(audit_entry_id),
  heat INTEGER NOT NULL CHECK (heat > 0),
  state TEXT NOT NULL CHECK (state IN ('containing', 'contained', 'dry_run', 'refused', 'uncertain', 'failed')),
  result_json TEXT,
  started_at TEXT NOT NULL,
  cooldown_until TEXT,
  completed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_containment_incident_executor
  ON containment_incidents (guild_id, executor_id, started_at);
