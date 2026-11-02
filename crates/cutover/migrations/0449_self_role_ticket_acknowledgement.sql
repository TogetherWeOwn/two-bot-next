-- Self-role ticket-row acknowledgement lifecycle (F1): durable columns on the
-- per-send `self_role_exchanges` tickets so ambiguous sends stop being
-- invisible. `settled_at`/`settled_as` record a later settlement decision
-- (`superseded_settled` when a replacement ticket takes over, `expired_unknown`
-- when the send ages out unresolved); `superseded_by` names the replacement
-- ticket; `attempt_no` counts wire sends for the ticket (one row means one
-- send); `outcome_class` carries the ambiguous outcome (`timeout_no_response`,
-- `response_ambiguous`, `loop_exhaustion`) and stays NULL for definite
-- outcomes. Additive and idempotent: re-applying is a no-op.
-- Rollback: ALTER TABLE self_role_exchanges DROP COLUMN settled_at,
-- DROP COLUMN settled_as, DROP COLUMN superseded_by, DROP COLUMN attempt_no,
-- DROP COLUMN outcome_class. Backout destroys acknowledgement state written
-- after apply; pre-existing tickets keep their dispositions.
ALTER TABLE self_role_exchanges
    ADD COLUMN IF NOT EXISTS settled_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS settled_as TEXT
        CHECK (settled_as IS NULL OR settled_as IN ('superseded_settled', 'expired_unknown')),
    ADD COLUMN IF NOT EXISTS superseded_by TEXT,
    ADD COLUMN IF NOT EXISTS attempt_no SMALLINT NOT NULL DEFAULT 1
        CHECK (attempt_no >= 1),
    ADD COLUMN IF NOT EXISTS outcome_class TEXT
        CHECK (outcome_class IS NULL OR outcome_class IN ('timeout_no_response', 'response_ambiguous', 'loop_exhaustion'));
