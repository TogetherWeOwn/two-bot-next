-- Captured member state and delivery receipts commit with the S3 sequence.
-- Interaction callback tokens never enter this table; their receipts identify
-- interrupted work after restart without replaying potentially partial writes.
CREATE TABLE IF NOT EXISTS gateway_onboarding_jobs (
    id bigserial PRIMARY KEY,
    guild_id text NOT NULL,
    shard_id integer NOT NULL CHECK (shard_id >= 0),
    session_id text NOT NULL,
    seq bigint NOT NULL CHECK (seq >= 0),
    occurred_at_ms bigint NOT NULL,
    payload text,
    state text NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'running', 'completed', 'interrupted', 'failed')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 3),
    UNIQUE (guild_id, shard_id, session_id, seq)
);
CREATE INDEX IF NOT EXISTS gateway_onboarding_jobs_pending
    ON gateway_onboarding_jobs (guild_id, shard_id, id)
    WHERE state IN ('pending', 'running', 'failed');
