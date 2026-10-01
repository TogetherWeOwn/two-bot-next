-- Recoverable, fenced self-role dispatches and cross-process exclusive-panel leases.
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS attempted_added_role_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS attempted_removed_role_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS compensated_added_role_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS compensated_removed_role_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS unresolved_added_role_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS unresolved_removed_role_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS desired_role_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS pre_mutation_role_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS claim_token TEXT;
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS claim_generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS processing_expires_at TIMESTAMPTZ;

-- Pre-lease rows did not persist enough intent to recover safely. Finalize
-- them as explicitly interrupted; a redelivered interaction can make a fresh,
-- authoritative request instead of treating [] as a fabricated desired state.
UPDATE self_role_audit
   SET outcome = 'rejected',
       code = 'interrupted_before_recovery',
       reason = 'processing row predates persisted self-role intent'
 WHERE outcome = 'processing' AND processing_expires_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_self_role_audit_processing_lease
  ON self_role_audit (outcome, processing_expires_at);

CREATE TABLE IF NOT EXISTS self_role_panel_claims (
  guild_id              TEXT NOT NULL,
  member_id             TEXT NOT NULL,
  panel_id              TEXT NOT NULL,
  claim_token           TEXT NOT NULL,
  claim_generation      INTEGER NOT NULL,
  processing_expires_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (guild_id, member_id, panel_id)
);
