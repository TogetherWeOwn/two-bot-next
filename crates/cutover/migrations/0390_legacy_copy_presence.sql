-- Legacy 0041 records whether the bot-floor scan was truncated. Preserve the
-- evidence bit at cutover rather than silently treating a partial scan as full.
-- 0310 intentionally ported only legacy 0004; extend it additively here.
-- The copier itself never applies migrations: provision this target separately.
ALTER TABLE presence_probe
    ADD COLUMN IF NOT EXISTS bot_floor_scan_truncated BOOLEAN NOT NULL DEFAULT FALSE;
