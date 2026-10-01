-- Keep bounded delivered-mirror reconciliation from sorting the full audit log.
CREATE INDEX IF NOT EXISTS idx_operational_audit_mirror_check
  ON operational_audit_log (mirror_checked_at, mirrored_at, entry_id)
  WHERE delivery_state = 'delivered'
    AND mirror_channel_id IS NOT NULL
    AND mirror_message_id IS NOT NULL;
