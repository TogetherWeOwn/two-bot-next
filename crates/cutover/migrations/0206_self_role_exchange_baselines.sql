-- Attribute unknown work before the first tracked send. Legacy uncertainty has
-- no invented exchange identity and cannot be retired by a ticket receipt.
CREATE TABLE IF NOT EXISTS self_role_exchange_baselines (
    event_id TEXT PRIMARY KEY REFERENCES self_role_audit(event_id) ON DELETE CASCADE,
    legacy_pending BOOLEAN NOT NULL DEFAULT FALSE,
    unresolved_added_role_ids TEXT NOT NULL DEFAULT '[]',
    unresolved_removed_role_ids TEXT NOT NULL DEFAULT '[]'
);

-- Upgrade conservatively: existing aggregate uncertainty might overlap tickets
-- from 0205, so none of it is inferred to be exclusively ticket-attributed.
INSERT INTO self_role_exchange_baselines
    (event_id,legacy_pending,unresolved_added_role_ids,unresolved_removed_role_ids)
SELECT event_id,
       exchange_pending OR unresolved_added_role_ids <> '[]' OR unresolved_removed_role_ids <> '[]',
       unresolved_added_role_ids,unresolved_removed_role_ids
FROM self_role_audit
ON CONFLICT (event_id) DO NOTHING;

ALTER TABLE self_role_exchanges
    ADD COLUMN IF NOT EXISTS retired_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS retired_generation INTEGER;

-- Replaying this migration must not recapture later ticket uncertainty as legacy.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conrelid = 'self_role_exchanges'::regclass
          AND conname = 'self_role_exchange_retirement_shape'
    ) THEN
        ALTER TABLE self_role_exchanges ADD CONSTRAINT self_role_exchange_retirement_shape
            CHECK ((retired_at IS NULL AND retired_generation IS NULL)
                OR (retired_at IS NOT NULL AND retired_generation IS NOT NULL
                    AND retired_generation > 0 AND disposition <> 'pending'));
    END IF;
END
$$;
