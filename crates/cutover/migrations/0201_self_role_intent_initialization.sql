-- Admission must precede a fresh REST snapshot. Distinguish a crash before
-- initialization from an immutable, intentionally empty panel target.
-- Existing legacy/domain-store rows already carry their original snapshots.
ALTER TABLE self_role_audit
    ADD COLUMN IF NOT EXISTS intent_initialized BOOLEAN NOT NULL DEFAULT TRUE;
