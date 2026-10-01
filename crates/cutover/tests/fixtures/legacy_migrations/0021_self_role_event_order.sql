-- Persist explicit event chronology so delayed older interactions cannot
-- supersede a newer exclusive-panel selection.
ALTER TABLE self_role_audit ADD COLUMN IF NOT EXISTS event_order TEXT;
ALTER TABLE self_role_panel_claims ADD COLUMN IF NOT EXISTS latest_event_order TEXT;
