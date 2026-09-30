-- 0016_automation_claims: upgrade the briefly shipped pre-lease automation
-- schema without editing immutable 0015 (TOG-1648).
--
-- Fresh installs already receive these columns from 0015. ADD IF NOT EXISTS is
-- intentionally a no-op there and repairs databases that created the tables
-- before scheduler/sticky claim fencing landed.

ALTER TABLE scheduled_messages ADD COLUMN IF NOT EXISTS claim_token TEXT;
ALTER TABLE scheduled_messages ADD COLUMN IF NOT EXISTS claimed_at TEXT;
ALTER TABLE sticky_messages ADD COLUMN IF NOT EXISTS claim_token TEXT;
ALTER TABLE sticky_messages ADD COLUMN IF NOT EXISTS claimed_at TEXT;
