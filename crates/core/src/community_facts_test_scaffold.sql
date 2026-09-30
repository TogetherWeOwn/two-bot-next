-- Test-only scaffold: the exact `community_facts` DDL from legacy two-bot
-- `migrations/0018_community_scorecard.sql`, inlined so the 0311 store test
-- runs before 0160 (TOG-10083, which owns this table) merges. At runtime
-- 0160 < 0311 applies first and this file is never executed. Delete this
-- scaffold when 0160 lands; the store test then applies 0160 + 0311.

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
