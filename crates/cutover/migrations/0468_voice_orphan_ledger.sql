-- Evidence only: failed creator compensation has no tracked room/reservation.
-- Identity is insert-once; append/oldest-out retention is serialized per guild
-- by PgRoomStore. No channel names, member IDs or remote error bodies are kept.
-- This table grants no channel deletion authority. Additive cutover migration;
-- rollback leaves this inert evidence intact when the older binary is restored.
CREATE TABLE voice_orphan_ledger (
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (reason = 'creator_compensation_failed'),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (guild_id, channel_id)
);
CREATE INDEX IF NOT EXISTS voice_orphan_ledger_newest
    ON voice_orphan_ledger (guild_id, recorded_at DESC, channel_id DESC);
