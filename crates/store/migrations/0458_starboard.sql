-- Starboard foundation. No Discord sends or configuration activation.
-- Mirrored in the cutover chain; either migration order is additive.
-- The starrer PK also supplies the (guild, message) recount index.
CREATE TABLE IF NOT EXISTS starboard_config (
    guild_id BIGINT PRIMARY KEY CHECK (guild_id > 0),
    board_channel_id BIGINT CHECK (board_channel_id > 0),
    threshold SMALLINT NOT NULL DEFAULT 3 CHECK (threshold BETWEEN 1 AND 50),
    emoji TEXT NOT NULL DEFAULT '⭐' CHECK (char_length(emoji) BETWEEN 1 AND 128),
    excluded_channel_ids BIGINT[] NOT NULL DEFAULT '{}',
    updated_by TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE IF NOT EXISTS starboard_starrers (
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    source_channel_id BIGINT NOT NULL CHECK (source_channel_id > 0),
    source_message_id BIGINT NOT NULL CHECK (source_message_id > 0),
    user_id BIGINT NOT NULL CHECK (user_id > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (guild_id, source_message_id, user_id)
);

CREATE TABLE IF NOT EXISTS starboard_posts (
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    source_message_id BIGINT NOT NULL CHECK (source_message_id > 0),
    source_channel_id BIGINT NOT NULL CHECK (source_channel_id > 0),
    board_channel_id BIGINT NOT NULL CHECK (board_channel_id > 0),
    board_message_id BIGINT CHECK (board_message_id > 0),
    star_count BIGINT NOT NULL DEFAULT 0 CHECK (star_count >= 0),
    state TEXT NOT NULL DEFAULT 'posted'
        CHECK (state IN ('posted', 'below_threshold', 'source_deleted')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (guild_id, source_message_id)
);
