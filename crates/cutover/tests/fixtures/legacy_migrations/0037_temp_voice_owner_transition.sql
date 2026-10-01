-- Persist the intended owner BEFORE changing Discord permissions. The old
-- owner remains owner_id until its grant is removed and the new grant succeeds.
-- A pending transition blocks new controls and is retried by sweep/reconcile.
ALTER TABLE temp_voice_channels ADD COLUMN IF NOT EXISTS pending_owner_id TEXT;
