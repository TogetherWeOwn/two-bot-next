-- 0002_internal_actions: the durable state behind POST /internal/actions.
--
-- Four tables, one per thing the endpoint could not do while it was
-- in-process only (docs/INTERNAL_ACTIONS.md §6):
--
--   internal_nonces         replay guard that survives a restart
--   internal_idempotency    idempotency_key -> stored result
--   internal_action_log     the durable audit trail (§4)
--   internal_discord_events event_key -> discord_event_id, for event.upsert
--
-- Same carry-overs as 0001: timestamps are TEXT holding ISO-8601 UTC, because
-- every comparison in the codebase is a lexicographic string compare and that
-- is correct for this format. Do not switch one table to timestamptz on its
-- own - see TOG-36.
--
-- NOTHING HERE HOLDS A REQUEST BODY. Not the announcement text, not an
-- access_token, not a signature. The idempotency table stores a *hash* of the
-- body so a reused key with different content can be caught, and a result
-- object that the endpoint itself built. That is deliberate and it is asserted
-- by a test - see docs/INTERNAL_ACTIONS.md §4.

-- ---------------------------------------------------------------------------
-- internal_nonces: one row per accepted nonce, swept after the TTL.
--
-- Keyed by (key_id, nonce) rather than nonce alone. A real replay is a
-- recording of a signed request, so it always carries the original's key id -
-- scoping the key catches every replay that exists, and stops one caller from
-- being able to burn a nonce value out from under another.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS internal_nonces (
  key_id  TEXT NOT NULL,
  nonce   TEXT NOT NULL,
  seen_at TEXT NOT NULL,
  PRIMARY KEY (key_id, nonce)
);

-- The sweep deletes by age, so it is the only index that earns its keep.
CREATE INDEX IF NOT EXISTS idx_internal_nonces_seen ON internal_nonces (seen_at);

-- ---------------------------------------------------------------------------
-- internal_idempotency: the "a timeout was a lie" table.
--
-- The website retries with the SAME Idempotency-Key and a NEW nonce. The first
-- request to arrive claims the row (state 'in_flight'); when it finishes it
-- writes the result and flips to 'done'. A retry that finds 'done' gets the
-- stored result back and makes no Discord call.
--
-- state is only ever 'in_flight' or 'done'. There is no 'failed': a failed
-- attempt DELETEs its own row, because the caller retrying a 502 should get a
-- real second attempt rather than a cached failure.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS internal_idempotency (
  key_id          TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  action          TEXT NOT NULL,
  request_hash    TEXT NOT NULL,   -- sha256 of the raw body. Never the body.
  state           TEXT NOT NULL,   -- 'in_flight' | 'done'
  outcome         TEXT,            -- the result's outcome, for the log line
  result_json     TEXT,            -- the response `result` object, verbatim
  claimed_at      TEXT NOT NULL,
  completed_at    TEXT,
  PRIMARY KEY (key_id, idempotency_key)
);

-- Reclaiming a stale in-flight row scans by state and age.
CREATE INDEX IF NOT EXISTS idx_internal_idem_state ON internal_idempotency (state, claimed_at);

-- ---------------------------------------------------------------------------
-- internal_action_log: who called, which action, what happened (§4).
--
-- One row per request, accepted or rejected, including the ones rejected
-- before we knew who was calling - key_id and action are nullable for exactly
-- that reason. This is the audit trail the issue asks for; the structured
-- stdout line still happens too, and neither is derived from the other.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS internal_action_log (
  request_id      TEXT PRIMARY KEY,
  key_id          TEXT,
  action          TEXT,
  idempotency_key TEXT,
  outcome         TEXT NOT NULL,   -- 'assigned', 'replayed_result', 'rejected', ...
  code            TEXT,            -- the §2 error code, NULL on success
  status          INTEGER NOT NULL,
  reason          TEXT,            -- logReason, our detail, never sent to the caller
  duration_ms     INTEGER NOT NULL,
  created_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_internal_log_time   ON internal_action_log (created_at);
CREATE INDEX IF NOT EXISTS idx_internal_log_action ON internal_action_log (action, created_at);
CREATE INDEX IF NOT EXISTS idx_internal_log_key    ON internal_action_log (key_id, created_at);

-- ---------------------------------------------------------------------------
-- internal_discord_events: the website's event_key -> Discord's event id.
--
-- This is what makes event.upsert an upsert. The website owns a stable key of
-- its own; we remember which Discord scheduled event we created for it, so the
-- same call creates once and modifies thereafter.
--
-- Keyed with guild_id because the id is only meaningful inside one guild.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS internal_discord_events (
  guild_id         TEXT NOT NULL,
  event_key        TEXT NOT NULL,
  discord_event_id TEXT NOT NULL,
  created_at       TEXT NOT NULL,
  updated_at       TEXT NOT NULL,
  PRIMARY KEY (guild_id, event_key)
);
