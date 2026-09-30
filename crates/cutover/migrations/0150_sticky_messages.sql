-- Sticky messages + automation audit log (TOG-10082, S4 slice of TOG-9809).
--
-- Ports legacy two-bot `migrations/0015_automations.sql` (TOG-1648, MEE6
-- custom commands + StickyBot parity) for the sticky surface only, plus
-- `0016_automation_claims.sql` claim columns folded in (fresh installs
-- already receive them from 0015 upstream). Sibling slices own the other
-- two 0015 tables: `automation_commands` (TOG-10080) and
-- `scheduled_messages` (TOG-10081) — those cards create their tables here
-- in their own reserved blocks, so this file deliberately carries neither.
--
-- `automation_audit_log` IS created here: every sticky write path appends
-- a `sticky.*` row (`sticky.create/update/delete/run`, outcomes
-- `ok/rejected/absent/post_failed`, ids and outcomes only — never member
-- content). `IF NOT EXISTS` throughout keeps parallel-slice merges safe.
--
-- Legacy names and checks preserved. Timestamps are timestamptz (legacy
-- `0009` converted the funnel tables the same way); the Rust store crosses
-- the boundary as millisecond Unix times via `to_timestamp()`.
-- Debounce 1–300 default 5 rides the `CHECK`, matching the slash-option
-- bounds in `crates/core/src/feature_commands.rs` and the service
-- validation in `crates/core/src/sticky.rs`.

CREATE TABLE IF NOT EXISTS sticky_messages (
  guild_id         TEXT NOT NULL,
  channel_id       TEXT NOT NULL,
  body             TEXT NOT NULL,
  -- Repost only after this many quiet seconds (StickyBot-style debounce).
  debounce_seconds INTEGER NOT NULL DEFAULT 5,
  enabled          BOOLEAN NOT NULL DEFAULT TRUE,
  last_message_id  TEXT,
  last_posted_at   timestamptz,
  created_by       TEXT NOT NULL,
  created_at       timestamptz NOT NULL,
  updated_by       TEXT NOT NULL,
  updated_at       timestamptz NOT NULL,
  claim_token      TEXT,
  claimed_at       timestamptz,
  PRIMARY KEY (guild_id, channel_id),
  CONSTRAINT sticky_messages_body CHECK (length(body) BETWEEN 1 AND 2000),
  CONSTRAINT sticky_messages_debounce CHECK (debounce_seconds BETWEEN 1 AND 300)
);

CREATE TABLE IF NOT EXISTS automation_audit_log (
  id         TEXT PRIMARY KEY,
  guild_id   TEXT NOT NULL,
  -- NULL for system actors: the sticky re-poster.
  actor_id   TEXT,
  action     TEXT NOT NULL,
  target_key TEXT,
  outcome    TEXT NOT NULL,
  reason     TEXT,
  created_at timestamptz NOT NULL
);

-- CREATE IF NOT EXISTS cannot upgrade an existing legacy 0015/0016 table.
-- Repair the pre-lease shape too, then convert all timestamp columns. Legacy
-- ISO-8601 strings carry UTC/offsets; the cast preserves the instant and NULLs
-- and fails on invalid data rather than silently discarding it. Casting an
-- already-timestamptz column is also safe (fresh install / DDL reapplication).
ALTER TABLE sticky_messages ADD COLUMN IF NOT EXISTS claim_token TEXT;
ALTER TABLE sticky_messages ADD COLUMN IF NOT EXISTS claimed_at timestamptz;

ALTER TABLE sticky_messages
  ALTER COLUMN last_posted_at TYPE timestamptz USING last_posted_at::timestamptz,
  ALTER COLUMN created_at TYPE timestamptz USING created_at::timestamptz,
  ALTER COLUMN updated_at TYPE timestamptz USING updated_at::timestamptz,
  ALTER COLUMN claimed_at TYPE timestamptz USING claimed_at::timestamptz;

ALTER TABLE automation_audit_log
  ALTER COLUMN created_at TYPE timestamptz USING created_at::timestamptz;

CREATE INDEX IF NOT EXISTS idx_automation_audit_guild_time
  ON automation_audit_log (guild_id, created_at);
