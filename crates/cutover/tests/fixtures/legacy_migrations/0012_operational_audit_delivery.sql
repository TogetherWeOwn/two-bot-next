-- Retryable delivery state for operational audit mirrors (TOG-1652).
--
-- The event row remains the durable source of truth. These columns make a
-- Discord delivery failure visible and retryable without inserting a second
-- event or redirecting it after configuration changes.
ALTER TABLE operational_audit_log
  ADD COLUMN IF NOT EXISTS mirror_channel_id TEXT,
  ADD COLUMN IF NOT EXISTS delivery_state TEXT NOT NULL DEFAULT 'none',
  ADD COLUMN IF NOT EXISTS delivery_attempts INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS delivery_attempted_at TIMESTAMPTZ,
  ADD COLUMN IF NOT EXISTS delivery_last_error TEXT,
  ADD COLUMN IF NOT EXISTS delivery_lease_until TIMESTAMPTZ,
  ADD COLUMN IF NOT EXISTS mirrored_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_operational_audit_delivery
  ON operational_audit_log (delivery_state, created_at);
