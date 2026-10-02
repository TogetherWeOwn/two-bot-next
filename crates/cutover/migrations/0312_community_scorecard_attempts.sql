-- TOG-11145: scheduler reservations, not scorecard output/revisions. Keep the
-- weekly budget in the same database as community_scorecard_runs so restarts,
-- timeouts and failed executions cannot reset it. Reserve before doing work.
CREATE TABLE IF NOT EXISTS community_scorecard_attempts (
  guild_id        TEXT NOT NULL,
  week_key        TEXT NOT NULL,
  attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 3),
  next_attempt_at BIGINT NOT NULL DEFAULT 0,
  completed       BOOLEAN NOT NULL DEFAULT FALSE,
  PRIMARY KEY (guild_id, week_key)
);
