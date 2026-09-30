-- S5 checkpoint: one shard per guild. No bot tokens or open voice sessions.
-- seq is the last dispatch whose funnel batch committed in the same transaction.
CREATE TABLE IF NOT EXISTS gateway_sessions (
    guild_id TEXT NOT NULL,
    shard_id INTEGER NOT NULL CHECK (shard_id >= 0),
    session_id TEXT NOT NULL,
    seq BIGINT NOT NULL CHECK (seq >= 0),
    resume_url TEXT NOT NULL,
    updated_at timestamptz NOT NULL,
    PRIMARY KEY (guild_id, shard_id)
);
