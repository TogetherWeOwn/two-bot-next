-- Terminal repair owns evidence separately from processing-event admission.
-- A completed repair never reopens the rejected event or publishes a target.
ALTER TABLE self_role_audit
    ADD COLUMN IF NOT EXISTS repair_expires_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS repair_complete BOOLEAN NOT NULL DEFAULT FALSE;

-- Keep discovery proportional to eligible work, not all terminal audit history.
CREATE INDEX IF NOT EXISTS self_role_audit_terminal_repair_due_idx
    ON self_role_audit (guild_id, panel_id, source_id, source,
                       repair_expires_at ASC NULLS FIRST, event_id COLLATE "C")
    WHERE outcome = 'rejected' AND code = 'superseded_by_later_event'
      AND NOT repair_complete
      AND (exchange_pending OR added_role_ids <> '[]' OR removed_role_ids <> '[]'
           OR attempted_added_role_ids <> '[]' OR attempted_removed_role_ids <> '[]'
           OR compensated_added_role_ids <> '[]' OR compensated_removed_role_ids <> '[]'
           OR unresolved_added_role_ids <> '[]' OR unresolved_removed_role_ids <> '[]');
