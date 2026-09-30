-- 0011_moderation_durability: the TOG-1659 High fixes.
--
--  * moderation_lockdowns: one row per locked channel, holding the @everyone
--    overwrite exactly as it was before Owen denied SendMessages. Without it,
--    unlock can only guess - and a guess that flips bits the moderators set
--    on purpose is a permissions breach in the other direction.
--  * moderation_idempotency: the atomic claim table for moderation verbs.
--    Same shape as internal_idempotency (0002), keyed by guild instead of
--    caller key id, because both entry paths - slash commands and signed
--    internal actions - land in the same ModerationService.execute().
--  * moderation_scheduled_unbans gets a partial unique index: at most one
--    pending unban per (guild, user). Re-banning or extending a tempban moves
--    the one job instead of forking a second, and a completed job stops
--    blocking the next tempban.

CREATE TABLE IF NOT EXISTS moderation_lockdowns (
  channel_id    TEXT PRIMARY KEY,
  guild_id      TEXT NOT NULL,
  prior_allow   TEXT NOT NULL,
  prior_deny    TEXT NOT NULL,
  reason        TEXT NOT NULL,
  locked_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_moderation_lockdowns_guild
  ON moderation_lockdowns (guild_id, locked_at);

-- claim column for the running state: when a running job's claim went stale,
-- the next sweep must be able to see how stale it was.
ALTER TABLE moderation_scheduled_unbans ADD COLUMN IF NOT EXISTS claimed_at TEXT;

CREATE TABLE IF NOT EXISTS moderation_idempotency (
  guild_id        TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  action          TEXT NOT NULL,
  request_hash    TEXT NOT NULL,
  state           TEXT NOT NULL,
  outcome         TEXT,
  result_json     TEXT,
  claimed_at      TEXT NOT NULL,
  completed_at    TEXT,
  PRIMARY KEY (guild_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_moderation_idem_state
  ON moderation_idempotency (state, claimed_at);

-- Partial: completed jobs do not block the next tempban of the same user.
CREATE UNIQUE INDEX IF NOT EXISTS uq_moderation_pending_unban
  ON moderation_scheduled_unbans (guild_id, user_id) WHERE state = 'pending';
