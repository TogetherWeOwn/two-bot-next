-- Metadata-only operational audit for Discord event parity (TOG-1652).
--
-- Message bodies, usernames and nicknames never enter this table. The target,
-- actor, message and channel snowflakes are enough to correlate an event with
-- Discord's own audit log without widening the privacy contract.
CREATE TABLE IF NOT EXISTS operational_audit_log (
  entry_id               TEXT PRIMARY KEY,
  event_kind             TEXT NOT NULL,
  guild_id               TEXT NOT NULL,
  occurred_at            TIMESTAMPTZ NOT NULL,
  actor_id                TEXT,
  target_id               TEXT,
  source_channel_id       TEXT,
  destination_channel_id  TEXT,
  message_id              TEXT,
  action                  TEXT,
  metadata_json           TEXT NOT NULL,
  created_at              TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_operational_audit_time
  ON operational_audit_log (guild_id, occurred_at);
CREATE INDEX IF NOT EXISTS idx_operational_audit_kind
  ON operational_audit_log (guild_id, event_kind, occurred_at);
CREATE INDEX IF NOT EXISTS idx_operational_audit_target
  ON operational_audit_log (guild_id, target_id, occurred_at);
