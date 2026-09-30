-- Durable ownership of permanent and temporary member bans (TOG-10078).
-- A prepared intent exists before dispatch; only explicit Discord acceptance
-- permits an expiry. Prepared/uncertain intents conservatively fence older
-- expiries until explicit reconciliation accepts or safely rejects them.
-- Generation is persisted insertion order, never wall-clock/request-id order.

CREATE TABLE IF NOT EXISTS moderation_member_bans (
  request_id   TEXT PRIMARY KEY,
  guild_id     TEXT NOT NULL,
  user_id      TEXT NOT NULL,
  generation   BIGSERIAL NOT NULL UNIQUE,
  state        TEXT NOT NULL CHECK (state IN ('prepared', 'accepted', 'rejected')),
  created_at   timestamptz NOT NULL,
  completed_at timestamptz
);
CREATE INDEX IF NOT EXISTS idx_moderation_member_bans_generation
  ON moderation_member_bans (guild_id, user_id, generation);

-- 0110's schedule state is unconstrained TEXT and already supports the new
-- 'quarantined' state. Imported deployments with a schedule-state CHECK or
-- enum must extend that constraint/type to include 'quarantined' BEFORE this
-- migration; do not remove the constraint or rewrite/delete historical data.
-- No foreign key/backfill can honestly infer whether an old Discord ban was
-- accepted. Preserve every row and its timestamps/claim token/reason for
-- explicit reconciliation, but never automatically recover these expiries.
-- Existing prepared intents are deliberately left uncertain; re-running DDL
-- must not convert them to accepted or activate their schedules.
UPDATE moderation_scheduled_unbans AS job
SET state = 'quarantined'
WHERE job.state IN ('staged', 'pending', 'running')
  AND NOT EXISTS (
    SELECT 1 FROM moderation_member_bans AS intent
    WHERE intent.request_id = job.request_id
      AND intent.guild_id = job.guild_id
      AND intent.user_id = job.user_id
  );
