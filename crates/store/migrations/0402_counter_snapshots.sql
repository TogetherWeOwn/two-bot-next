-- 0005_counter_snapshots: the bot-owned audit trail behind the latest live count.
--
-- The website never reads this table. `web_v1.live_counts` continues to read the
-- single latest cache row in `guild_counters`; this history exists so the bot can
-- prove which aggregate it observed and when without retaining any member list.
--
-- Both clocks are independent. The member and online counts do not require the
-- same Discord capability and do not have the same freshness ceiling, so a read
-- of one must never make the other look fresh.
CREATE TABLE IF NOT EXISTS counter_snapshots (
  guild_id              TEXT PRIMARY KEY,
  human_member_count    INTEGER,
  human_member_count_at TEXT,
  online_count          INTEGER,
  online_count_at       TEXT,

  CONSTRAINT counter_snapshots_members_nonneg
    CHECK (human_member_count IS NULL OR human_member_count >= 0),
  CONSTRAINT counter_snapshots_online_nonneg
    CHECK (online_count IS NULL OR online_count >= 0),
  CONSTRAINT counter_snapshots_members_dated
    CHECK ((human_member_count IS NULL) = (human_member_count_at IS NULL)),
  CONSTRAINT counter_snapshots_online_dated
    CHECK ((online_count IS NULL) = (online_count_at IS NULL))
);

-- Latest aggregate-only classification used by every public member projection.
-- This does not retain Discord role rows or names. It records only that a member
-- belongs to the dynamically-derived raid exclusion set, so `web_v1.members`
-- cannot count a raid account that the landing-page counter excluded.
CREATE TABLE IF NOT EXISTS member_exclusions (
  guild_id   TEXT NOT NULL,
  member_id  TEXT NOT NULL,
  reason     TEXT NOT NULL CHECK (reason = 'raid'),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, member_id)
);
