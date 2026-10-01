-- 0017_scheduled_occurrence_nonce: persist the Discord idempotency key for a
-- scheduled occurrence so ambiguous-response retries reuse it after the lease
-- token changes or the process restarts (TOG-1648).

ALTER TABLE scheduled_messages ADD COLUMN IF NOT EXISTS occurrence_nonce TEXT;
