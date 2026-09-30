-- 0310_presence_probe: the internal-only presence instrument (TOG-10092, S5).
--
-- Mirrors legacy two-bot `migrations/0004_presence_probe.sql` verbatim for the
-- table this slice owns. Read that migration's header before changing anything
-- here: this instrument exists only because it needs no new gateway intent,
-- stores no per-member row, and never reaches a page. If a change breaks one
-- of those three, the change is wrong rather than the rule.
--
-- Type notes (legacy-faithful, do not "fix" in this slice):
-- * `observed_at` is TEXT holding ISO-8601 UTC (legacy 0004); ordering and
--   the `(guild_id, observed_at)` key compare lexicographically, correct for
--   that format.
-- * `bot_floor` is NULL on most rows ("not rescanned"); a reader wanting the
--   floor for a reading takes the most recent non-NULL at or before it.
-- * There is deliberately NO `human_estimate` column (see legacy 0004).
--
-- `community_scorecard_runs/alerts`, `community_facts` and
-- `community_stream_heartbeats` belong to 0311; `community_facts` itself is
-- already ported inside 0160 (TOG-10083, host check-in facts) and must not be
-- recreated here. `CREATE TABLE / INDEX IF NOT EXISTS` throughout: S6
-- (TOG-9811) applies the same legacy DDL when it ports 0004 in full, and a
-- re-run must never fail on DDL. Legacy table/column names are kept exactly.

CREATE TABLE IF NOT EXISTS presence_probe (
  guild_id                   TEXT NOT NULL,
  -- ISO-8601 UTC, matching every other timestamp in this codebase.
  observed_at                TEXT NOT NULL,

  -- Straight from Discord. Bots included - that is the known defect, and it is
  -- why nothing may render this without subtracting a floor it has verified.
  approximate_presence_count INTEGER NOT NULL,

  -- Members with `user.bot` true at the time of the reading. NULLABLE, and
  -- NULL is the common case: the collector rescans at most once a day and
  -- writes NULL in between.
  bot_floor                  INTEGER,

  PRIMARY KEY (guild_id, observed_at),

  CONSTRAINT presence_probe_count_nonneg
    CHECK (approximate_presence_count >= 0),
  CONSTRAINT presence_probe_floor_nonneg
    CHECK (bot_floor IS NULL OR bot_floor >= 0)
);

-- The only access pattern: one guild, newest first.
CREATE INDEX IF NOT EXISTS idx_presence_probe_observed
  ON presence_probe (guild_id, observed_at);
