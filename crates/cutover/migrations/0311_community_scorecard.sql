-- 0311_community_scorecard: durable human-only scorecards (TOG-10092, S5).
--
-- Mirrors legacy two-bot `migrations/0018_community_scorecard.sql` for the
-- tables this slice owns: `community_stream_heartbeats`,
-- `community_scorecard_runs`, and `community_scorecard_alerts`. Raw facts are
-- content-minimized and append-only. Scorecard revisions and alert dedupe rows
-- remain auditable; no recommendation path may delete source facts. Rota
-- extensions (legacy 0028-0033) stay dropped with the rota stack.
--
-- `community_facts` itself is NOT recreated here: it already lands in 0160
-- (TOG-10083, host check-in facts). This file only adds the tables 0160 does
-- not own. `CREATE TABLE / INDEX IF NOT EXISTS` throughout: S6 (TOG-9811)
-- applies the same legacy DDL when it ports 0018 in full, and a re-run must
-- never fail on DDL. Legacy table/column names are kept exactly.
--
-- Type notes (legacy-faithful, do not "fix" in this slice):
-- * `occurred_at`/`covered_from`/`covered_through`/`week_start`/`week_end`
--   are TEXT holding ISO-8601 UTC (legacy 0018); every comparison is a
--   lexicographic string compare, correct for that format.

CREATE TABLE IF NOT EXISTS community_stream_heartbeats (
  guild_id        TEXT NOT NULL,
  stream          TEXT NOT NULL CHECK (stream IN (
    'message_created', 'voice_session_started', 'voice_session_ended',
    'member_joined', 'event_attended', 'rules_accepted'
  )),
  covered_from    TEXT NOT NULL,
  covered_through TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  PRIMARY KEY (guild_id, stream),
  CHECK (covered_from <= covered_through)
);

CREATE TABLE IF NOT EXISTS community_scorecard_runs (
  id                 BIGSERIAL PRIMARY KEY,
  guild_id           TEXT NOT NULL,
  week_start         TEXT NOT NULL,
  week_end           TEXT NOT NULL,
  classifier_version TEXT NOT NULL,
  watermark          BIGINT NOT NULL,
  input_count        INTEGER NOT NULL,
  input_hash         TEXT NOT NULL,
  idempotency_key    TEXT NOT NULL UNIQUE,
  revision           INTEGER NOT NULL,
  run_status         TEXT NOT NULL CHECK (run_status IN ('completed', 'incomplete')),
  coverage_state     TEXT NOT NULL CHECK (coverage_state IN ('complete', 'incomplete')),
  evidence_state     TEXT NOT NULL CHECK (evidence_state IN ('sufficient', 'insufficient')),
  scorecard_json     TEXT NOT NULL,
  intervention_code  TEXT NOT NULL,
  generated_at       TEXT NOT NULL,
  UNIQUE (guild_id, week_start, classifier_version, revision)
);

CREATE INDEX IF NOT EXISTS idx_community_scorecard_runs_week
  ON community_scorecard_runs (guild_id, week_start, classifier_version, revision DESC);

CREATE TABLE IF NOT EXISTS community_scorecard_alerts (
  guild_id  TEXT NOT NULL,
  week_start TEXT NOT NULL,
  alert_key TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, alert_key)
);
