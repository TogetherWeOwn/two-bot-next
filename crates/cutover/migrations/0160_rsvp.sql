-- 0160_rsvp: scheduled-event RSVPs, host check-in facts, RSVP audit (TOG-10083).
--
-- Mirrors legacy two-bot `migrations/0024_announcements_feeds.sql` (TOG-1649)
-- for the two tables this slice owns — `event_rsvps` and
-- `announcements_audit_log` — plus `migrations/0018_community_scorecard.sql`
-- for `community_facts`, which host check-in appends `event_attended` facts
-- to. The rest of 0024 (`lfg_posts/roles/signups`, `feed_relays/deliveries`)
-- belongs to TOG-10084/TOG-10085, and the scorecard runs/alerts/heartbeats
-- belong to S5 (TOG-10092); they must not be recreated here.
--
-- `CREATE TABLE IF NOT EXISTS` throughout: S6 (TOG-9811) applies the same
-- legacy DDL when it ports 0018/0024 in full, and a re-run must never fail
-- on DDL. Legacy table/column names are kept exactly so RSVP rows stay
-- consistent with what two-web-next reads.
--
-- Type notes (legacy-faithful, do not "fix" in this slice):
-- * `event_rsvps.responded_at` and `announcements_audit_log.created_at` are
--   timestamptz; the store always writes ISO-8601 UTC, which Postgres parses
--   exactly.
-- * `community_facts.occurred_at` is TEXT holding ISO-8601 UTC (legacy 0018);
--   every comparison is a lexicographic string compare, correct for that
--   format.

CREATE TABLE IF NOT EXISTS event_rsvps (
  guild_id    TEXT NOT NULL,
  event_id    TEXT NOT NULL,
  user_id     TEXT NOT NULL,
  status      TEXT NOT NULL CHECK (status IN ('going', 'interested', 'declined')),
  responded_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (guild_id, event_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_event_rsvps_event
  ON event_rsvps (guild_id, event_id, status);

CREATE TABLE IF NOT EXISTS announcements_audit_log (
  id          TEXT PRIMARY KEY,
  guild_id    TEXT NOT NULL,
  actor_id    TEXT,
  action      TEXT NOT NULL,
  target_key  TEXT,
  outcome     TEXT NOT NULL,
  reason      TEXT,
  created_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_announcements_audit_guild_time
  ON announcements_audit_log (guild_id, created_at);

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
