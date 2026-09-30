-- Serialize channel mutations across requests and replicas. No lease expiry:
-- an ambiguous Discord outcome must block a new key as well as its own retry.
-- A proven-safe claim release cascades; successful completion removes the guard
-- atomically with its audit/recovery cleanup in the runtime store API.
CREATE TABLE moderation_channel_execution (
  guild_id        TEXT NOT NULL,
  channel_id      TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  claim_token     TEXT NOT NULL,
  PRIMARY KEY (guild_id, channel_id),
  FOREIGN KEY (guild_id, idempotency_key)
    REFERENCES moderation_idempotency (guild_id, idempotency_key) ON DELETE CASCADE
);
