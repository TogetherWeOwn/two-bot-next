-- Persist the newest accepted exclusive-panel event so recovery never revives
-- an older intent after a later selection completed.
ALTER TABLE self_role_panel_claims ADD COLUMN IF NOT EXISTS latest_event_id TEXT;
ALTER TABLE self_role_panel_claims ADD COLUMN IF NOT EXISTS latest_option_key TEXT;
