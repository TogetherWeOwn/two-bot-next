-- Guild-fenced website event-key to Discord scheduled-event ID map.
-- The signed event.read verifier resolves the caller's event_key in the
-- request guild through this table; an unmapped key is refused
-- action_not_allowed before any Discord call. The same mapping is written by
-- event.upsert after Discord confirms a create (legacy discordEventId).
-- No member identity: guild_id names the fence, event_id the mapped resource.
CREATE TABLE internal_event_keys (
    guild_id TEXT NOT NULL CHECK (guild_id ~ '^[0-9]{17,20}$'),
    event_key TEXT NOT NULL CHECK (event_key ~ '^[A-Za-z0-9._:-]{1,200}$'),
    event_id TEXT NOT NULL CHECK (event_id ~ '^[0-9]{17,20}$'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (guild_id, event_key)
);
