-- Member moderation ledger (TOG-10078, S4 member slice).
--
-- Mirrors legacy two-bot `0010_moderation.sql` (+ `0011_moderation_durability`
-- idempotency/claim columns and `0012_moderation_recovery` claim ownership)
-- for the member-slice tables only: warnings, the tempban unban queue, the
-- audit ledger, and the idempotency claim table. `moderation_lockdowns`
-- belongs to the channel slice (TOG-10079) and lands in its own file inside
-- that card's reserved block, so the two slices never collide.
--
-- Legacy table/column names are kept verbatim. Timestamps are timestamptz
-- (repo convention from 0001/0002; legacy stored ISO TEXT) and the store
-- binds ISO-8601 UTC strings with `::timestamptz` casts, same as `db.rs`.
--
-- `CREATE TABLE / INDEX IF NOT EXISTS` throughout: the tools run against
-- staging and agent-testdb scratch schemas that may already carry these
-- tables, and a migration must never fail a re-run on DDL.

CREATE TABLE IF NOT EXISTS moderation_warnings (
  id          TEXT PRIMARY KEY,
  guild_id    TEXT NOT NULL,
  user_id     TEXT NOT NULL,
  actor_id    TEXT NOT NULL,
  reason      TEXT NOT NULL,
  request_id  TEXT NOT NULL UNIQUE,
  created_at  timestamptz NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_moderation_warnings_user
  ON moderation_warnings (guild_id, user_id, created_at);

CREATE TABLE IF NOT EXISTS moderation_scheduled_unbans (
  request_id   TEXT PRIMARY KEY,
  guild_id     TEXT NOT NULL,
  user_id      TEXT NOT NULL,
  execute_at   timestamptz NOT NULL,
  reason       TEXT NOT NULL,
  state        TEXT NOT NULL,
  created_at   timestamptz NOT NULL,
  completed_at timestamptz,
  claimed_at   timestamptz,
  claim_token  TEXT
);
CREATE INDEX IF NOT EXISTS idx_moderation_unbans_due
  ON moderation_scheduled_unbans (state, execute_at);

-- Partial: completed jobs do not block the next tempban of the same user.
-- Re-banning or extending a tempban moves the one pending job instead of
-- forking a second (legacy `uq_moderation_pending_unban`).
CREATE UNIQUE INDEX IF NOT EXISTS uq_moderation_pending_unban
  ON moderation_scheduled_unbans (guild_id, user_id) WHERE state = 'pending';

CREATE TABLE IF NOT EXISTS moderation_audit (
  request_id      TEXT PRIMARY KEY,
  guild_id        TEXT NOT NULL,
  actor_id        TEXT NOT NULL,
  action          TEXT NOT NULL,
  target_id       TEXT,
  channel_id      TEXT,
  reason          TEXT NOT NULL,
  outcome         TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  metadata_json   TEXT NOT NULL,
  created_at      timestamptz NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_moderation_audit_time
  ON moderation_audit (guild_id, created_at);
CREATE INDEX IF NOT EXISTS idx_moderation_audit_target
  ON moderation_audit (guild_id, target_id, created_at);

-- Atomic claim table: one row per (guild, key) BEFORE any Discord mutation.
-- Same shape as `internal_idempotency` (legacy 0002), keyed by guild instead
-- of caller key id, because both entry paths — slash commands and signed
-- internal actions — land in the same execute path.
CREATE TABLE IF NOT EXISTS moderation_idempotency (
  guild_id        TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  action          TEXT NOT NULL,
  request_hash    TEXT NOT NULL,
  state           TEXT NOT NULL,
  outcome         TEXT,
  result_json     TEXT,
  claimed_at      timestamptz NOT NULL,
  completed_at    timestamptz,
  PRIMARY KEY (guild_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_moderation_idem_state
  ON moderation_idempotency (state, claimed_at);
