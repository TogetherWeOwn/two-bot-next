-- A truncated roster has no floor, but it still consumed the daily scan budget.
-- Persist only that outcome alongside the aggregate reading so restarts do not
-- re-list an oversized guild hourly. Existing NULL floors remain unclassified;
-- transient read failures continue to retry on the next collection cycle.
ALTER TABLE presence_probe
  ADD COLUMN IF NOT EXISTS bot_floor_scan_truncated BOOLEAN NOT NULL DEFAULT FALSE;
