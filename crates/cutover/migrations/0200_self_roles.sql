-- Self-role schema at frozen legacy main d5d11793 (0018–0023).
-- Preserve legacy names and JSON-array-as-TEXT representation for cutover.
CREATE TABLE IF NOT EXISTS self_role_audit (
    event_id TEXT PRIMARY KEY,
    guild_id TEXT NOT NULL,
    panel_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    option_key TEXT,
    role_id TEXT,
    source TEXT NOT NULL,
    operation TEXT NOT NULL,
    outcome TEXT NOT NULL,
    code TEXT,
    reason TEXT,
    added_role_ids TEXT NOT NULL,
    removed_role_ids TEXT NOT NULL,
    created_at TEXT NOT NULL,
    attempted_added_role_ids TEXT NOT NULL DEFAULT '[]',
    attempted_removed_role_ids TEXT NOT NULL DEFAULT '[]',
    compensated_added_role_ids TEXT NOT NULL DEFAULT '[]',
    compensated_removed_role_ids TEXT NOT NULL DEFAULT '[]',
    unresolved_added_role_ids TEXT NOT NULL DEFAULT '[]',
    unresolved_removed_role_ids TEXT NOT NULL DEFAULT '[]',
    desired_role_ids TEXT NOT NULL DEFAULT '[]',
    pre_mutation_role_ids TEXT NOT NULL DEFAULT '[]',
    claim_token TEXT,
    claim_generation INTEGER NOT NULL DEFAULT 0,
    processing_expires_at TIMESTAMPTZ,
    event_order TEXT
);
CREATE INDEX IF NOT EXISTS idx_self_role_audit_panel_time
    ON self_role_audit (guild_id, panel_id, created_at);
CREATE INDEX IF NOT EXISTS idx_self_role_audit_member_time
    ON self_role_audit (guild_id, member_id, created_at);
CREATE INDEX IF NOT EXISTS idx_self_role_audit_processing_lease
    ON self_role_audit (outcome, processing_expires_at);

CREATE TABLE IF NOT EXISTS self_role_panel_claims (
    guild_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    panel_id TEXT NOT NULL,
    claim_token TEXT NOT NULL,
    claim_generation INTEGER NOT NULL,
    processing_expires_at TIMESTAMPTZ NOT NULL,
    latest_event_id TEXT,
    latest_option_key TEXT,
    latest_event_order TEXT,
    target_committed BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (guild_id, member_id, panel_id)
);
