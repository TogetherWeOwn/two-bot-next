-- Durable, per-panel audit for reaction/button/select self-role changes (TOG-1646).
--
-- One row per Discord dispatch. event_id is the interaction id for components
-- and a generated dispatch id for reactions, which Discord does not identify.
-- Component gateway replays are claimed before any role mutation.
CREATE TABLE IF NOT EXISTS self_role_audit (
  event_id          TEXT PRIMARY KEY,
  guild_id          TEXT NOT NULL,
  panel_id          TEXT NOT NULL,
  member_id         TEXT NOT NULL,
  source_id         TEXT NOT NULL,
  option_key        TEXT,
  role_id           TEXT,
  source            TEXT NOT NULL,
  operation         TEXT NOT NULL,
  outcome           TEXT NOT NULL,
  code              TEXT,
  reason            TEXT,
  added_role_ids    TEXT NOT NULL,
  removed_role_ids  TEXT NOT NULL,
  created_at        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_self_role_audit_panel_time
  ON self_role_audit (guild_id, panel_id, created_at);
CREATE INDEX IF NOT EXISTS idx_self_role_audit_member_time
  ON self_role_audit (guild_id, member_id, created_at);
