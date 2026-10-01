-- 0300_website_contract: bot-owned tables behind the `web_v1` read contract
-- (TOG-10090).
--
-- Ports legacy two-bot `migrations/0003_web_contract_tables.sql` (+ the
-- `0005_counter_snapshots.sql` audit/exclusion tables) into this card's
-- reserved block (0300–0309) so parallel S5 slices never collide. Table and
-- column names are the legacy names unchanged: the `web_v1` views in
-- `sql/web_v1.sql` (the two-web-next read contract) resolve them through
-- `search_path`.
--
-- This file creates TABLES only. The views the website reads live in
-- `sql/web_v1.sql` and are applied by `website_store::apply_web_contract`,
-- not by a migration: a contract view is edited over its life (a v1.1 adds a
-- column) and a migration is immutable by rule.
--
-- Timestamps on these tables are TEXT holding ISO-8601 UTC, matching legacy
-- 0003/0005: their readers are string comparisons that are correct for the
-- format, and the views cast on the way out (`web_v1._ts` tolerates a bad
-- row as NULL rather than failing the page). The funnel tables (`events`,
-- `members`) are real timestamptz (0001, legacy 0009); the views render them
-- back as ISO text via `web_v1._iso`.
--
-- `CREATE TABLE IF NOT EXISTS` throughout, matching the cutover convention:
-- one-shots and staging boots re-run migrations against schemas that may
-- already carry these tables.

-- Single row. The version the views in this database implement, so the
-- website can assert what it built against instead of guessing.
CREATE TABLE IF NOT EXISTS web_contract_meta (
  singleton        BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
  contract_version TEXT NOT NULL,
  -- Set by the bot once it knows which guild it is in. Until then the
  -- contract_meta view derives it from the data. NULL is not a failure.
  guild_id         TEXT
);

INSERT INTO web_contract_meta (singleton, contract_version)
VALUES (TRUE, '1.0')
ON CONFLICT (singleton) DO NOTHING;

-- The landing page counters. One row per guild; the contract is single-guild.
CREATE TABLE IF NOT EXISTS guild_counters (
  guild_id              TEXT PRIMARY KEY,
  human_member_count    INTEGER,
  human_member_count_at TEXT,
  online_count          INTEGER,
  online_count_at       TEXT,

  CONSTRAINT guild_counters_members_nonneg
    CHECK (human_member_count IS NULL OR human_member_count >= 0),
  CONSTRAINT guild_counters_online_nonneg
    CHECK (online_count IS NULL OR online_count >= 0),

  -- The zero rule, enforced in the schema and not only in the collector: a
  -- count and the time it was read move together or not at all. A count with
  -- no read time cannot be aged out, so it would eventually be published as
  -- fresh forever. This constraint makes that row impossible to write rather
  -- than merely discouraged.
  CONSTRAINT guild_counters_members_dated
    CHECK ((human_member_count IS NULL) = (human_member_count_at IS NULL)),
  CONSTRAINT guild_counters_online_dated
    CHECK ((online_count IS NULL) = (online_count_at IS NULL))
);

-- The Prospect -> Legend ladder. A table rather than a CHECK constraint so
-- the five rows always exist: `rank_counts` left-joins from here, which is
-- what guarantees a rank never vanishes from the ladder just because nobody
-- holds it.
--
-- role_id is the Discord snowflake, filled in by the rank collector. It stays
-- on this side of the contract on purpose: the website never hardcodes a
-- snowflake.
CREATE TABLE IF NOT EXISTS rank_ladder (
  rank_key   TEXT PRIMARY KEY,
  rank_label TEXT NOT NULL,
  rank_order INTEGER NOT NULL UNIQUE,
  role_id    TEXT
);

INSERT INTO rank_ladder (rank_key, rank_label, rank_order) VALUES
  ('prospect', 'Prospect', 1),
  ('member',   'Member',   2),
  ('soldier',  'Soldier',  3),
  ('veteran',  'Veteran',  4),
  ('legend',   'Legend',   5)
ON CONFLICT (rank_key) DO NOTHING;

-- Latest headcount per rank. Two columns because the ranks stack: a Legend
-- still holds Soldier, Member and Prospect.
--   member_count  - people whose HIGHEST rank is this one. Mutually exclusive.
--   holders_count - people holding the role at all. Cumulative.
-- The ladder on the website uses member_count; holders_count sums to more
-- than the membership and does not survive a visitor adding up the columns.
CREATE TABLE IF NOT EXISTS rank_snapshots (
  guild_id      TEXT NOT NULL,
  rank_key      TEXT NOT NULL REFERENCES rank_ladder (rank_key),
  member_count  INTEGER,
  holders_count INTEGER,
  snapshot_at   TEXT NOT NULL,
  PRIMARY KEY (guild_id, rank_key),

  CONSTRAINT rank_snapshots_member_count_nonneg
    CHECK (member_count IS NULL OR member_count >= 0),
  CONSTRAINT rank_snapshots_holders_count_nonneg
    CHECK (holders_count IS NULL OR holders_count >= 0)
);

-- Highest rank per member, for profile pages. No row means no rank role,
-- which is the honest state for a third of the server today.
CREATE TABLE IF NOT EXISTS member_ranks (
  guild_id   TEXT NOT NULL,
  member_id  TEXT NOT NULL,
  rank_key   TEXT REFERENCES rank_ladder (rank_key),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, member_id)
);

-- Discord scheduled events, mirrored so the landing page never waits on the
-- Discord API. Zero rows is a real and expected answer.
CREATE TABLE IF NOT EXISTS scheduled_events (
  guild_id    TEXT NOT NULL,
  event_id    TEXT NOT NULL,
  name        TEXT NOT NULL,
  starts_at   TEXT NOT NULL,
  channel_id  TEXT,
  description TEXT,
  -- 'scheduled' | 'active' | 'completed' | 'cancelled', mirroring Discord.
  -- Only 'scheduled' and 'active' reach the contract views.
  status      TEXT NOT NULL DEFAULT 'scheduled',
  updated_at  TEXT NOT NULL,
  PRIMARY KEY (guild_id, event_id)
);

CREATE INDEX IF NOT EXISTS idx_scheduled_events_start
  ON scheduled_events (guild_id, starts_at);

-- The bot-owned audit trail behind the latest live count.
--
-- The website never reads this table. `web_v1.live_counts` continues to read
-- the single latest cache row in `guild_counters`; this history exists so the
-- bot can prove which aggregate it observed and when without retaining any
-- member list.
--
-- Both clocks are independent. The member and online counts do not require
-- the same Discord capability and do not have the same freshness ceiling, so
-- a read of one must never make the other look fresh.
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

-- Latest aggregate-only classification used by every public member
-- projection. This does not retain Discord role rows or names. It records
-- only that a member belongs to the dynamically-derived raid exclusion set,
-- so `web_v1.members` cannot count a raid account that the landing-page
-- counter excluded.
CREATE TABLE IF NOT EXISTS member_exclusions (
  guild_id   TEXT NOT NULL,
  member_id  TEXT NOT NULL,
  reason     TEXT NOT NULL CHECK (reason = 'raid'),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, member_id)
);
