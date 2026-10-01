-- Serialize channel effects across workers and entry points. An uncertain
-- Discord mutation retains this fence: no expiry may let a delayed unlock
-- overwrite a newer lockdown. Reconciliation must prove the old effect settled.
CREATE TABLE moderation_channel_executions (
  channel_id      TEXT PRIMARY KEY,
  guild_id        TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  claim_token     TEXT NOT NULL,
  FOREIGN KEY (guild_id, idempotency_key)
    REFERENCES moderation_idempotency (guild_id, idempotency_key)
);
