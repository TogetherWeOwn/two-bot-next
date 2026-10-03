-- 0141: upgrade an existing legacy queue without changing the locked 0140 DDL.
-- CREATE TABLE IF NOT EXISTS cannot widen INTEGER or add columns to old tables.
-- Keep every definition, timestamp, claim and interval CHECK intact; BIGINT
-- makes both legacy recurring intervals and NULL one-shots decode as sqlx i64.
ALTER TABLE scheduled_messages
  ALTER COLUMN interval_seconds TYPE BIGINT USING interval_seconds::BIGINT;

-- Support legacy queues from before either the claim or occurrence-nonce slice.
ALTER TABLE scheduled_messages
  ADD COLUMN IF NOT EXISTS claim_token TEXT,
  ADD COLUMN IF NOT EXISTS claimed_at TEXT,
  ADD COLUMN IF NOT EXISTS occurrence_nonce TEXT;
