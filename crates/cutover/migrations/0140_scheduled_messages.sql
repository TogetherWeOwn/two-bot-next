-- 0140_scheduled_messages: scheduled-message queue + automation audit (TOG-10081).
--
-- Ports legacy two-bot `migrations/0015_automations.sql` (+ `0016` claim
-- fencing, `0017` occurrence nonce) for the tables the S4 scheduled-messages
-- slice needs: `scheduled_messages` (the 15 s ticker queue, `next_run_at`
-- ordered) and `automation_audit_log` (ids-and-outcomes trail, never message
-- content). Custom-command and sticky tables land with their own slices
-- (TOG-10080, TOG-10082); the audit table is shared, created here with
-- `IF NOT EXISTS` so parallel slices never collide.
--
-- Keeps legacy table/column names AND the legacy TEXT ISO-8601 timestamps
-- (`Date.toISOString()` shape, always millis). Fixed-width by construction,
-- so the ticker's lexicographic `next_run_at` ordering is chronological.
-- One deliberate widening: `interval_seconds` is BIGINT where legacy is
-- INTEGER — the 60–31_536_000 CHECK is unchanged, and BIGINT maps to sqlx
-- `i64` with no casts at the store boundary.
-- `CREATE TABLE IF NOT EXISTS` throughout, matching 0001/0002: one-shots and
-- re-runs must never fail on DDL.

CREATE TABLE IF NOT EXISTS scheduled_messages (
  id               TEXT PRIMARY KEY,
  guild_id         TEXT NOT NULL,
  channel_id       TEXT NOT NULL,
  body             TEXT NOT NULL,
  -- ISO-8601 UTC. One-shot when interval_seconds is NULL.
  next_run_at      TEXT NOT NULL,
  interval_seconds BIGINT,
  enabled          BOOLEAN NOT NULL DEFAULT TRUE,
  last_run_at      TEXT,
  last_message_id  TEXT,
  created_by       TEXT NOT NULL,
  created_at       TEXT NOT NULL,
  updated_by       TEXT NOT NULL,
  updated_at       TEXT NOT NULL,
  claim_token      TEXT,
  claimed_at       TEXT,
  occurrence_nonce TEXT,
  CONSTRAINT scheduled_messages_body CHECK (length(body) BETWEEN 1 AND 2000),
  CONSTRAINT scheduled_messages_interval CHECK (
    interval_seconds IS NULL OR interval_seconds BETWEEN 60 AND 31536000
  )
);

-- The scheduler polls this. Partial index: disabled rows never come due.
CREATE INDEX IF NOT EXISTS idx_scheduled_messages_due
  ON scheduled_messages (enabled, next_run_at);

CREATE TABLE IF NOT EXISTS automation_audit_log (
  id         TEXT PRIMARY KEY,
  guild_id   TEXT NOT NULL,
  -- NULL for system actors (the scheduler ticker).
  actor_id   TEXT,
  action     TEXT NOT NULL,
  target_key TEXT,
  outcome    TEXT NOT NULL,
  reason     TEXT,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_automation_audit_guild_time
  ON automation_audit_log (guild_id, created_at);
