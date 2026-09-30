-- Channel moderation recovery state and the shared audit/idempotency ledger.
-- Legacy table/column names; mask strings retain the exact prior overwrite.
-- IF NOT EXISTS allows the member-moderation slice to share the ledger tables.

CREATE TABLE IF NOT EXISTS moderation_lockdowns (
  channel_id   TEXT PRIMARY KEY,
  guild_id     TEXT NOT NULL,
  prior_allow  TEXT NOT NULL,
  prior_deny   TEXT NOT NULL,
  prior_exists BOOLEAN NOT NULL,
  reason       TEXT NOT NULL,
  locked_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_moderation_lockdowns_guild
  ON moderation_lockdowns (guild_id, locked_at);

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

CREATE TABLE IF NOT EXISTS moderation_idempotency (
  guild_id        TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  action          TEXT NOT NULL,
  request_hash    TEXT NOT NULL,
  state           TEXT NOT NULL,
  outcome         TEXT,
  result_json     TEXT,
  claimed_at      TEXT NOT NULL,
  completed_at    TEXT,
  PRIMARY KEY (guild_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_moderation_idem_state
  ON moderation_idempotency (state, claimed_at);
