-- Pin the role actually attempted, independently of later role-key remapping.
-- Existing intents remain NULL: a digest cannot reconstruct their resolved role.
ALTER TABLE internal_idempotency
    ADD COLUMN resolved_role_id TEXT CHECK (resolved_role_id ~ '^[0-9]{17,20}$');
ALTER TABLE internal_action_log
    ADD COLUMN resolved_role_id TEXT CHECK (resolved_role_id ~ '^[0-9]{17,20}$');
