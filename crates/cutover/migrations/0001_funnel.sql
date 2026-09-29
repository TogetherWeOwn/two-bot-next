-- Cutover funnel log and projections (TOG-9882).
--
-- Mirrors legacy two-bot `migrations/0001_initial.sql` (+ `0007` gate_cleared
-- and `0008` third_message_at) for the tables the import/backfill path
-- writes. Timestamps are timestamptz (legacy `0009` converted the funnel
-- tables); the cutover tools always write ISO-8601 UTC, which Postgres
-- parses exactly. `is_bot` is BOOLEAN (legacy `0009`).
--
-- `CREATE TABLE IF NOT EXISTS` throughout: the tools run against staging and
-- agent-testdb scratch schemas that may already carry the S6-ported tables,
-- and a one-shot must never fail a re-run on DDL.

CREATE TABLE IF NOT EXISTS events (
  id              BIGSERIAL PRIMARY KEY,
  event_type      TEXT NOT NULL,
  member_id       TEXT,
  guild_id        TEXT NOT NULL,
  occurred_at     timestamptz NOT NULL,
  recorded_at     timestamptz NOT NULL DEFAULT date_trunc('milliseconds', now()),
  source          TEXT NOT NULL,
  metadata        TEXT,
  idempotency_key TEXT NOT NULL UNIQUE
);

CREATE INDEX IF NOT EXISTS idx_events_type_time ON events (event_type, occurred_at);
CREATE INDEX IF NOT EXISTS idx_events_member    ON events (guild_id, member_id, event_type);
CREATE INDEX IF NOT EXISTS idx_events_source    ON events (source, occurred_at);

CREATE TABLE IF NOT EXISTS members (
  guild_id            TEXT NOT NULL,
  member_id           TEXT NOT NULL,
  joined_at           timestamptz,
  join_source         TEXT,
  first_message_at    timestamptz,
  -- No second_message_at by design (legacy TWO-95): the middle rung is a
  -- log marker only, derivable from `events`, so `members` stays a pure
  -- projection.
  third_message_at    timestamptz,
  first_voice_at      timestamptz,
  last_active_at      timestamptz,
  left_at             timestamptz,
  inactive_flagged_at timestamptz,
  gate_cleared_at     timestamptz,
  is_bot              BOOLEAN NOT NULL DEFAULT FALSE,
  PRIMARY KEY (guild_id, member_id)
);

CREATE INDEX IF NOT EXISTS idx_members_joined  ON members (guild_id, joined_at);
CREATE INDEX IF NOT EXISTS idx_members_lastact ON members (guild_id, last_active_at);

CREATE TABLE IF NOT EXISTS invite_snapshots (
  guild_id   TEXT NOT NULL,
  code       TEXT NOT NULL,
  uses       INTEGER NOT NULL,
  inviter_id TEXT,
  channel_id TEXT,
  updated_at timestamptz NOT NULL,
  PRIMARY KEY (guild_id, code)
);
