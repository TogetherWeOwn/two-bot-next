-- 0001_initial: the funnel log and its projections.
--
-- Ported from the SQLite schema (src/store/schema.sql) as part of TWO-18.
-- Two deliberate carry-overs, so that this is a driver swap and not a
-- behaviour change:
--
--  * Timestamps are TEXT holding ISO-8601 UTC, not timestamptz. Every
--    comparison in the codebase is a lexicographic string compare, which is
--    correct for that format, and Date.parse() reads them back. Converting to
--    timestamptz is a real improvement but it is a separate change with its
--    own tests - see TWO-36.
--  * is_bot is SMALLINT 0/1, not BOOLEAN, because the queries say `is_bot = 0`.
--
-- Both are safe for the website team to read. Cast on the way out if you want
-- real timestamps: occurred_at::timestamptz.

CREATE TABLE IF NOT EXISTS events (
  id              BIGSERIAL PRIMARY KEY,
  event_type      TEXT NOT NULL,
  member_id       TEXT,            -- discord snowflake, NULL only for pre-join events
  guild_id        TEXT NOT NULL,
  occurred_at     TEXT NOT NULL,   -- ISO-8601 UTC, set by the emitter
  recorded_at     TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
  source          TEXT NOT NULL,
  metadata        TEXT,            -- JSON blob, keep small (docs/PRIVACY.md)
  idempotency_key TEXT NOT NULL UNIQUE
);

CREATE INDEX IF NOT EXISTS idx_events_type_time ON events (event_type, occurred_at);
CREATE INDEX IF NOT EXISTS idx_events_member    ON events (guild_id, member_id, event_type);
CREATE INDEX IF NOT EXISTS idx_events_source    ON events (source, occurred_at);

CREATE TABLE IF NOT EXISTS members (
  guild_id            TEXT NOT NULL,
  member_id           TEXT NOT NULL,
  joined_at           TEXT,
  join_source         TEXT,
  first_message_at    TEXT,
  first_voice_at      TEXT,
  last_active_at      TEXT,
  left_at             TEXT,
  inactive_flagged_at TEXT,
  is_bot              SMALLINT NOT NULL DEFAULT 0,
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
  updated_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, code)
);
