-- A strict claim remains occupied until its exact generation settles.
ALTER TABLE public.discord_send_admission
    ADD COLUMN IF NOT EXISTS pending_settlement_generation BIGINT,
    DROP CONSTRAINT IF EXISTS discord_send_admission_pending_settlement_check,
    ADD CONSTRAINT discord_send_admission_pending_settlement_check
        CHECK (pending_settlement_generation IS NULL OR
               (in_flight AND pending_settlement_generation = generation));
