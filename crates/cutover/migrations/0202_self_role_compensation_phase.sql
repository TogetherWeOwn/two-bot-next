-- Rollback is a durable, monotonic decision, not inferred from effect arrays.
-- Preserve the legacy names and immutable before/desired snapshots.
ALTER TABLE self_role_audit
    ADD COLUMN IF NOT EXISTS compensating BOOLEAN NOT NULL DEFAULT FALSE;
