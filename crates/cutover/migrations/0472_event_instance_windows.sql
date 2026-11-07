-- Durable, non-member evidence from accepted Discord event observations.
-- No legacy timestamp backfill: a binding alone cannot prove a live window.
-- The mirror remains exact-replacement; omission is not terminal evidence.
-- A reschedule changes starts_at/ends_at, never instance_starts_at.
-- Keep revocation/order fences while a mapping or binding can reference them.
-- Additive rollout: old binaries ignore this table. Roll back the binary,
-- not these fences; schema removal needs a separate reviewed migration.
CREATE TABLE event_instance_windows (
    guild_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    instance_starts_at TIMESTAMPTZ NOT NULL,
    starts_at TIMESTAMPTZ NOT NULL,
    ends_at TIMESTAMPTZ,
    status TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    omitted_at TEXT,
    revoked_at TEXT,
    is_current BOOLEAN NOT NULL DEFAULT TRUE,
    identity_ambiguous BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (guild_id, event_id, instance_starts_at)
);
CREATE UNIQUE INDEX event_instance_windows_current
    ON event_instance_windows (guild_id, event_id) WHERE is_current;
