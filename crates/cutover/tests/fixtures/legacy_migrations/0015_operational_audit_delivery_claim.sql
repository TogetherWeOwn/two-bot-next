-- Fence audit mirror delivery writes to the worker that owns the current lease.
-- Every claim and reclaim rotates this token; stale workers cannot renew, release,
-- or acknowledge the replacement worker's delivery.
ALTER TABLE operational_audit_log
  ADD COLUMN IF NOT EXISTS delivery_claim_token TEXT;
