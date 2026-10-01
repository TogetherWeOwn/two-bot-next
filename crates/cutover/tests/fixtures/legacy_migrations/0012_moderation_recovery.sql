-- 0012_moderation_recovery: safe scheduled-unban claim ownership and a
-- deployable pending-unban uniqueness constraint for databases that already
-- contain duplicate rows from 0010.

ALTER TABLE moderation_scheduled_unbans
  ADD COLUMN IF NOT EXISTS claim_token TEXT;

ALTER TABLE moderation_lockdowns
  ADD COLUMN IF NOT EXISTS prior_exists BOOLEAN NOT NULL DEFAULT TRUE;

-- 0010 allowed multiple pending rows for one member. Keep the latest expiry as
-- the active schedule and retire the rest before creating the unique index.
WITH ranked AS (
  SELECT request_id,
         row_number() OVER (
           PARTITION BY guild_id, user_id
           ORDER BY execute_at DESC, created_at DESC, request_id DESC
         ) AS position
    FROM moderation_scheduled_unbans
   WHERE state = 'pending'
)
UPDATE moderation_scheduled_unbans AS jobs
   SET state = 'superseded',
       completed_at = COALESCE(jobs.completed_at, CURRENT_TIMESTAMP::text)
  FROM ranked
 WHERE jobs.request_id = ranked.request_id
   AND ranked.position > 1;

CREATE UNIQUE INDEX IF NOT EXISTS uq_moderation_pending_unban
  ON moderation_scheduled_unbans (guild_id, user_id) WHERE state = 'pending';
