-- A snapshot does not prove that a timed-out or interrupted exchange has stopped.
-- Journal before send; clear only on a definite no-send or a received response.
-- Recovery must retain unknown remote work and never publish false success.
ALTER TABLE self_role_audit
    ADD COLUMN IF NOT EXISTS exchange_pending BOOLEAN NOT NULL DEFAULT FALSE;
