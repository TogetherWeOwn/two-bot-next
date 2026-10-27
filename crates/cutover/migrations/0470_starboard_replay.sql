-- Durable per-user, per-gateway-session order, including remove tombstones.
-- Session hashes are identities, not clocks; compare sequence only within one.
CREATE TABLE IF NOT EXISTS starboard_reaction_cursors (
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    source_message_id BIGINT NOT NULL CHECK (source_message_id > 0),
    user_id BIGINT NOT NULL CHECK (user_id > 0),
    session_key TEXT NOT NULL,
    event_order TEXT COLLATE "C" NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (guild_id, source_message_id, user_id, session_key)
);
