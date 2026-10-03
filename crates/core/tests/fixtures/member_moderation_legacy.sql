-- Legacy member ledger after durability/recovery migrations: ISO TEXT timestamps.
CREATE TABLE moderation_warnings (
  id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  reason TEXT NOT NULL,
  request_id TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL
);
CREATE TABLE moderation_scheduled_unbans (
  request_id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  execute_at TEXT NOT NULL,
  reason TEXT NOT NULL,
  state TEXT NOT NULL,
  created_at TEXT NOT NULL,
  completed_at TEXT,
  claimed_at TEXT,
  claim_token TEXT
);
CREATE TABLE moderation_audit (
  request_id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  action TEXT NOT NULL,
  target_id TEXT,
  channel_id TEXT,
  reason TEXT NOT NULL,
  outcome TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  metadata_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE moderation_idempotency (
  guild_id TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  action TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  state TEXT NOT NULL,
  outcome TEXT,
  result_json TEXT,
  claimed_at TEXT NOT NULL,
  completed_at TEXT,
  PRIMARY KEY (guild_id, idempotency_key)
);
