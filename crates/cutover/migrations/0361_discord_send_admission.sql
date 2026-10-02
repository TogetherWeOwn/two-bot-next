-- Deliberately database-wide, not search_path/guild/transport scoped. Each
-- credential has one lane; no raw tokens, request contents or provider text.
-- No expiry on in_flight: process loss requires explicit reconciliation.
CREATE TABLE IF NOT EXISTS public.discord_send_admission (
    token_key TEXT PRIMARY KEY CHECK (token_key ~ '^[0-9a-f]{64}$'),
    generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
    in_flight BOOLEAN NOT NULL DEFAULT FALSE,
    indefinite BOOLEAN NOT NULL DEFAULT FALSE,
    hold_until_ms BIGINT NOT NULL DEFAULT 0 CHECK (hold_until_ms >= 0)
);
