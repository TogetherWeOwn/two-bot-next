-- Bounded single-question polls. Existing tables and rows are untouched.
CREATE TABLE IF NOT EXISTS polls (
    guild_id TEXT NOT NULL,
    id TEXT NOT NULL,
    question TEXT NOT NULL CHECK (char_length(question) BETWEEN 1 AND 256),
    options_json JSONB NOT NULL CHECK (jsonb_typeof(options_json) = 'array' AND jsonb_array_length(options_json) BETWEEN 2 AND 10),
    channel_id TEXT NOT NULL,
    message_id TEXT,
    created_by TEXT NOT NULL,
    ends_at TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed')),
    max_voters BIGINT CHECK (max_voters BETWEEN 1 AND 1000),
    winner_index BIGINT CHECK (winner_index BETWEEN 0 AND 9),
    closed_at TIMESTAMPTZ,
    results_posted_at TIMESTAMPTZ,
    results_started_at TIMESTAMPTZ,
    results_message_id TEXT,
    counts_json JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, id)
);
CREATE INDEX IF NOT EXISTS polls_due ON polls (guild_id, ends_at)
    WHERE status = 'open';
CREATE INDEX IF NOT EXISTS polls_unposted ON polls (guild_id, closed_at)
    WHERE status = 'closed' AND results_posted_at IS NULL;
CREATE TABLE IF NOT EXISTS poll_votes (
    guild_id TEXT NOT NULL,
    poll_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    option_index BIGINT NOT NULL CHECK (option_index BETWEEN 0 AND 9),
    voted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, poll_id, user_id),
    FOREIGN KEY (guild_id, poll_id) REFERENCES polls (guild_id, id) ON DELETE CASCADE
);
