-- TOG-10085: feed-owned block 0180-0189. Legacy table/column names.
CREATE TABLE IF NOT EXISTS feed_relays (
    id TEXT PRIMARY KEY,
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('rss', 'youtube', 'twitch')),
    source TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    last_checked_at TIMESTAMPTZ,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    UNIQUE (guild_id, channel_id, kind, source)
);

CREATE TABLE IF NOT EXISTS feed_deliveries (
    feed_id TEXT NOT NULL REFERENCES feed_relays(id) ON DELETE CASCADE,
    item_key TEXT NOT NULL,
    nonce TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'delivered')),
    message_id TEXT,
    first_seen_at TIMESTAMPTZ NOT NULL,
    delivered_at TIMESTAMPTZ,
    claim_token TEXT,
    claimed_at TIMESTAMPTZ,
    PRIMARY KEY (feed_id, item_key),
    UNIQUE (feed_id, nonce)
);
-- Also valid against a restored pre-claims legacy schema.
ALTER TABLE feed_deliveries ADD COLUMN IF NOT EXISTS claim_token TEXT;
ALTER TABLE feed_deliveries ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ;

-- Shared announcements audit table, also installed by 0160_rsvp.
CREATE TABLE IF NOT EXISTS announcements_audit_log (
    id TEXT PRIMARY KEY,
    guild_id TEXT NOT NULL,
    actor_id TEXT,
    action TEXT NOT NULL,
    target_key TEXT,
    outcome TEXT NOT NULL,
    reason TEXT,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_announcements_audit_guild_time
    ON announcements_audit_log (guild_id, created_at);
