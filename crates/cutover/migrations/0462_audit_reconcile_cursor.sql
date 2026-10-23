-- Durable floor of fully inspected audit recovery history. NULL starts at head;
-- never replaces the original pre-POST boundary or acceptance evidence.
-- Additive and replay-safe. Roll back the binary with this column retained:
-- dropping it would discard progress and break cursor-aware readers.
ALTER TABLE IF EXISTS operational_audit_log
    ADD COLUMN IF NOT EXISTS delivery_reconcile_before TEXT;
