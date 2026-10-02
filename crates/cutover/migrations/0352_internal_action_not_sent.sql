-- Only an owning executor's explicit proof of no dispatch enables same-key retry.
-- Unknown/stale ordinary intents remain reconciliation-only; retain all audit rows.
ALTER TABLE internal_idempotency DROP CONSTRAINT internal_idempotency_state_check;
ALTER TABLE internal_idempotency ADD CONSTRAINT internal_idempotency_state_check
    CHECK (state IN ('in_flight', 'unknown', 'completed', 'not_sent'));

ALTER TABLE internal_action_log DROP CONSTRAINT internal_action_log_phase_check;
ALTER TABLE internal_action_log ADD CONSTRAINT internal_action_log_phase_check
    CHECK (phase IN ('intent', 'unknown', 'terminal', 'released'));
