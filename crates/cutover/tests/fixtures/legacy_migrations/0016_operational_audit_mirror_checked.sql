-- Bounded runtime verification of already-delivered operational-audit mirrors.
-- NULL means the exact Discord message has not yet been checked by this build.
ALTER TABLE operational_audit_log
  ADD COLUMN IF NOT EXISTS mirror_checked_at TIMESTAMPTZ;
