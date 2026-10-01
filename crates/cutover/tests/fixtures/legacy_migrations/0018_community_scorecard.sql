-- 0018_community_scorecard: durable human-only Community Platform scorecards.
-- Raw facts are content-minimized and append-only. Scorecard revisions and alert
-- dedupe rows remain auditable; no recommendation path may delete source facts.

CREATE TABLE IF NOT EXISTS community_facts (
  id                 BIGSERIAL PRIMARY KEY,
  guild_id           TEXT NOT NULL,
  event_type         TEXT NOT NULL CHECK (event_type IN (
    'message_created', 'voice_session_started', 'voice_session_ended',
    'member_joined', 'event_attended', 'rules_accepted'
  )),
  source_event_id    TEXT NOT NULL,
  actor_id           TEXT,
  occurred_at        TEXT NOT NULL,
  recorded_at        TEXT NOT NULL DEFAULT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
  source             TEXT NOT NULL,
  classifier_version TEXT NOT NULL,
  classification     TEXT NOT NULL CHECK (classification IN (
    'eligible_human', 'bot', 'webhook', 'staff_automation', 'raid', 'staging', 'test'
  )),
  matched_rule       TEXT NOT NULL,
  metadata           TEXT,
  idempotency_key    TEXT NOT NULL UNIQUE
);

CREATE INDEX IF NOT EXISTS idx_community_facts_guild_week
  ON community_facts (guild_id, occurred_at, id);
CREATE INDEX IF NOT EXISTS idx_community_facts_type_class
  ON community_facts (guild_id, event_type, classification, occurred_at);

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
