-- Durable guards only: unknown/stale intents are never reclaimed automatically.
CREATE TABLE internal_nonces (
    nonce_hash TEXT PRIMARY KEY CHECK (nonce_hash ~ '^[0-9a-f]{64}$'),
    burned_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    CHECK (expires_at >= burned_at + INTERVAL '241 seconds')
);

CREATE TABLE internal_idempotency (
    intent_id BIGSERIAL PRIMARY KEY,
    caller_hash TEXT NOT NULL CHECK (caller_hash ~ '^[0-9a-f]{64}$'),
    key_hash TEXT NOT NULL CHECK (key_hash ~ '^[0-9a-f]{64}$'),
    action TEXT NOT NULL CHECK (action IN (
        'role.assign', 'guild.add_member', 'announcement.post',
        'event.upsert', 'event.cancel', 'event.read',
        'automations.import', 'automations.export', 'settings.get', 'settings.set',
        'moderation.ban', 'moderation.tempban', 'moderation.kick',
        'moderation.timeout', 'moderation.warn', 'moderation.purge',
        'moderation.slowmode', 'moderation.lockdown', 'moderation.unlock'
    )),
    payload_hash TEXT NOT NULL CHECK (payload_hash ~ '^[0-9a-f]{64}$'),
    state TEXT NOT NULL CHECK (state IN ('in_flight', 'unknown', 'completed')),
    guild_id TEXT CHECK (guild_id ~ '^[0-9]{17,20}$'),
    actor_id TEXT CHECK (actor_id ~ '^[0-9]{17,20}$'),
    target_id TEXT CHECK (target_id ~ '^[0-9]{17,20}$'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    response_code TEXT CHECK (response_code IN (
        'success', 'malformed', 'action_not_allowed', 'discord_rejected', 'no_effect'
    )),
    http_status INTEGER,
    resource_id TEXT CHECK (resource_id ~ '^[0-9]{17,20}$'),
    affected BIGINT CHECK (affected BETWEEN 0 AND 4294967295),
    UNIQUE (caller_hash, key_hash),
    CHECK (
        (state <> 'completed' AND response_code IS NULL AND http_status IS NULL
            AND resource_id IS NULL AND affected IS NULL)
        OR (state = 'completed' AND response_code IS NOT NULL AND http_status IS NOT NULL
            AND (
                (response_code = 'success' AND http_status = 200 AND affected IS NOT NULL)
                OR (response_code = 'malformed' AND http_status = 400 AND resource_id IS NULL AND affected IS NULL)
                OR (response_code = 'action_not_allowed' AND http_status = 403 AND resource_id IS NULL AND affected IS NULL)
                OR (response_code = 'discord_rejected' AND http_status = 422 AND resource_id IS NULL AND affected IS NULL)
                OR (response_code = 'no_effect' AND http_status = 502 AND resource_id IS NULL AND affected IS NULL)
            ))
    )
);
CREATE INDEX internal_idempotency_pending ON internal_idempotency (created_at)
    WHERE state <> 'completed';

-- The ledger contains only scalars copied from a validated intent/response.
-- No request/response JSON, OAuth token, raw key/nonce, header or free-text detail.
CREATE TABLE internal_action_log (
    audit_id BIGSERIAL PRIMARY KEY,
    intent_id BIGINT NOT NULL REFERENCES internal_idempotency (intent_id),
    phase TEXT NOT NULL CHECK (phase IN ('intent', 'unknown', 'terminal')),
    caller_hash TEXT NOT NULL CHECK (caller_hash ~ '^[0-9a-f]{64}$'),
    action TEXT NOT NULL,
    guild_id TEXT CHECK (guild_id ~ '^[0-9]{17,20}$'),
    actor_id TEXT CHECK (actor_id ~ '^[0-9]{17,20}$'),
    target_id TEXT CHECK (target_id ~ '^[0-9]{17,20}$'),
    response_code TEXT,
    http_status INTEGER,
    evidence_code TEXT CHECK (evidence_code IN (
        'executor', 'discord_confirmed_effect', 'discord_confirmed_no_effect', 'proven_not_sent'
    )),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (intent_id, phase)
);

CREATE TABLE internal_discord_events (
    event_hash TEXT PRIMARY KEY CHECK (event_hash ~ '^[0-9a-f]{64}$'),
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
