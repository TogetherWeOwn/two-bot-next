-- 0435_retention_queues: rolling-window retention queues.
--
-- Backs the retention job's rolling windows (raw presence/voice rows roll off
-- at 30 days, audit-log cache at 45 days), the true-GUILD_DELETE purge queue,
-- and the user-deletion request ledger with under-72-hour receipts. Queues
-- hold scope only; time-window deletes read the existing tables. The retained
-- control-plane exceptions in docs/privacy.md stay retained: this migration
-- adds no delete path for guild_settings, guild_settings_audit,
-- audit_kill_switch, member_erasure_audit, or the operator-released
-- moderation_idempotency replay fence.
--
-- Rollback: DROP TABLE IF EXISTS retention_deletion_requests;
-- DROP TABLE IF EXISTS retention_guild_purge_queue; (queued guilds and
-- pending requests are lost; time-window rows already deleted stay deleted).
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS retention_guild_purge_queue (
  guild_id TEXT PRIMARY KEY,
  enqueued_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE IF NOT EXISTS retention_deletion_requests (
  request_id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  requested_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'completed')),
  completed_at TIMESTAMPTZ,
  receipt_id TEXT,
  CHECK (
    (status = 'pending' AND completed_at IS NULL)
    OR (status = 'completed' AND completed_at IS NOT NULL AND receipt_id IS NOT NULL)
  )
);

CREATE INDEX IF NOT EXISTS idx_retention_deletion_pending
  ON retention_deletion_requests (status, requested_at);
CREATE INDEX IF NOT EXISTS idx_retention_deletion_scope
  ON retention_deletion_requests (guild_id, user_id);
