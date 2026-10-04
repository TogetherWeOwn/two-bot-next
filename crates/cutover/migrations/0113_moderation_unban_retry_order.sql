-- Definite DELETE refusals yield a durable queue position to other due members.
-- Reuse the monotonic ownership sequence for queue tickets, never changing a
-- PUT's generation or using wall time as remote-outcome evidence. NULL means
-- this expiry retains its original intent-generation position.
ALTER TABLE moderation_scheduled_unbans
    ADD COLUMN IF NOT EXISTS retry_generation BIGINT
    CHECK (retry_generation > 0);
