-- Terminal audits name the affected resource for event intents (and every
-- other verb's receipt): an event.upsert create claims with no target, so the
-- audit trail otherwise cannot name the Discord event it created. Copies only
-- validated scalars from the completed receipt; existing rows stay NULL.
-- Backout: drop the three columns (terminal audits lose their resource
-- witness until re-recorded; the columns hold no member PII beyond the
-- receipt's own snowflakes and counts).
ALTER TABLE internal_action_log
    ADD COLUMN resource_id TEXT CHECK (resource_id ~ '^[0-9]{17,20}$'),
    ADD COLUMN affected BIGINT CHECK (affected BETWEEN 0 AND 4294967295),
    ADD COLUMN outcome TEXT CHECK (outcome IN ('created', 'updated', 'cancelled'));
