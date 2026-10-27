-- Keep the one-replacement budget with the immutable occurrence key. A
-- reschedule or process restart must not give a deleted replacement another
-- create. Only a confirmed replacement receipt consumes the budget; refused
-- POSTs may retry, while unknown POSTs retain the existing owner lease.
-- Nullable and no backfill: existing bindings have no recorded replacement.
ALTER TABLE event_voice_channels
    ADD COLUMN IF NOT EXISTS recreated_at TIMESTAMPTZ;
