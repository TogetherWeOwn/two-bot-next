-- Record the Discord message which satisfied an operational audit mirror.
--
-- The row is claimed before send with a deterministic nonce. Discord enforces
-- that nonce for retries, and the returned message id proves which post was
-- acknowledged when the durable row is marked delivered.
ALTER TABLE operational_audit_log
  ADD COLUMN IF NOT EXISTS delivery_nonce TEXT,
  ADD COLUMN IF NOT EXISTS mirror_message_id TEXT;
