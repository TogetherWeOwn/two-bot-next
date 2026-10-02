-- One immutable send identity, independent of rotating event/lane ownership.
-- A former worker may complete ONLY its own receipt, never an audit or target.
CREATE TABLE IF NOT EXISTS self_role_exchanges (
    exchange_id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    event_id TEXT NOT NULL REFERENCES self_role_audit(event_id),
    origin_generation INTEGER NOT NULL CHECK (origin_generation > 0),
    role_id TEXT NOT NULL CHECK (role_id ~ '^[0-9]+$'),
    adding BOOLEAN NOT NULL,
    compensating BOOLEAN NOT NULL,
    receipt_token TEXT NOT NULL DEFAULT gen_random_uuid()::text,
    disposition TEXT NOT NULL DEFAULT 'pending',
    response_status SMALLINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    completed_at TIMESTAMPTZ,
    CHECK (
        (disposition = 'pending' AND response_status IS NULL AND completed_at IS NULL)
        OR (disposition = 'no_send' AND response_status IS NULL AND completed_at IS NOT NULL)
        OR (disposition = 'response' AND response_status IS NOT NULL
            AND response_status BETWEEN 200 AND 599 AND completed_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS self_role_exchanges_event_idx
    ON self_role_exchanges (event_id, exchange_id);
CREATE INDEX IF NOT EXISTS self_role_exchanges_pending_idx
    ON self_role_exchanges (event_id) WHERE disposition = 'pending';
