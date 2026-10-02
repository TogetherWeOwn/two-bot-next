-- One-shot operator directive per gateway checkpoint key. While armed
-- (consumed_at IS NULL) the next boot IDENTIFYs whatever the checkpoint's age.
-- The boot read consumes it; nothing here deletes or rewrites gateway_sessions.
CREATE TABLE IF NOT EXISTS gateway_boot_directives (
    guild_id TEXT NOT NULL,
    shard_id INTEGER NOT NULL CHECK (shard_id >= 0),
    armed_at timestamptz NOT NULL,
    reason TEXT NOT NULL CHECK (length(btrim(reason)) BETWEEN 1 AND 512),
    consumed_at timestamptz,
    PRIMARY KEY (guild_id, shard_id)
);
