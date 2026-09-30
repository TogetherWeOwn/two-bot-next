-- S5 audit storage (TOG-10344). Reserve 0340–0349 for audit only.
-- Checked against main and the S5 delivery ledger: jobs 0300–0309,
-- probe/scorecard 0310–0319, sessions 0320–0329, settings 0330–0339.
-- Legacy names and meanings from two-bot 0011–0017 + 0027 audit migrations.
-- No message bodies, names, free-form reasons, jobs or gateway sessions.
CREATE TABLE IF NOT EXISTS operational_audit_log (
    entry_id TEXT PRIMARY KEY,
    event_kind TEXT NOT NULL,
    guild_id TEXT NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL,
    actor_id TEXT,
    target_id TEXT,
    source_channel_id TEXT,
    destination_channel_id TEXT,
    message_id TEXT,
    action TEXT,
    metadata_json TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

ALTER TABLE operational_audit_log
    ADD COLUMN IF NOT EXISTS mirror_channel_id TEXT,
    ADD COLUMN IF NOT EXISTS delivery_state TEXT NOT NULL DEFAULT 'none',
    ADD COLUMN IF NOT EXISTS delivery_attempts INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS delivery_attempted_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS delivery_last_error TEXT,
    ADD COLUMN IF NOT EXISTS delivery_lease_until TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS mirrored_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS delivery_nonce TEXT,
    ADD COLUMN IF NOT EXISTS mirror_message_id TEXT,
    ADD COLUMN IF NOT EXISTS delivery_search_before TEXT,
    ADD COLUMN IF NOT EXISTS delivery_claim_token TEXT,
    ADD COLUMN IF NOT EXISTS mirror_checked_at TIMESTAMPTZ,
    -- Each new owner also advances a monotonic fence. Neither is public input.
    ADD COLUMN IF NOT EXISTS delivery_generation BIGINT NOT NULL DEFAULT 0,
    -- Discord's returned/reconciled message ID is persisted before completion.
    ADD COLUMN IF NOT EXISTS delivery_accepted_at TIMESTAMPTZ,
    -- Preflight-only release parks the row out of queue discovery until this
    -- bound expires. Retry scheduling only, never a POST attempt count; a
    -- later preparation clears it.
    ADD COLUMN IF NOT EXISTS delivery_deferred_until TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_operational_audit_time
    ON operational_audit_log (guild_id, occurred_at);
CREATE INDEX IF NOT EXISTS idx_operational_audit_kind
    ON operational_audit_log (guild_id, event_kind, occurred_at);
CREATE INDEX IF NOT EXISTS idx_operational_audit_target
    ON operational_audit_log (guild_id, target_id, occurred_at);
CREATE INDEX IF NOT EXISTS idx_operational_audit_delivery
    ON operational_audit_log (delivery_state, created_at);
CREATE INDEX IF NOT EXISTS idx_operational_audit_mirror_check
    ON operational_audit_log (delivery_state, mirror_checked_at, mirrored_at, entry_id);

-- Presence, not a cached boolean, is the operator halt. First engagement wins.
CREATE TABLE IF NOT EXISTS audit_kill_switch (
    id INTEGER PRIMARY KEY,
    engaged_at TIMESTAMPTZ NOT NULL,
    engaged_by TEXT NOT NULL
);
