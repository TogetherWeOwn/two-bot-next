-- 0004_presence_probe: the internal-only presence instrument (TOG-469).
--
-- WHAT THIS IS
-- ------------
-- A slow time series of the guild's `approximate_presence_count`, read from
-- the REST API. Its only job is to answer one question with evidence instead
-- of with a single reading: is the number of humans online ever large enough
-- to be worth publishing?
--
-- TOG-75 decided C - no presence intent, `online_count` stays null - on the
-- strength of ONE observation (27 at the 19-Aug audit against a 23-bot floor,
-- i.e. single-digit humans). This table replaces that one observation with a
-- series, so C expires on evidence rather than standing forever by default.
--
-- WHY THIS IS ALLOWED WHEN THE PRESENCE INTENT WAS NOT
-- ----------------------------------------------------
-- Three properties, all of which must stay true:
--
--   1. No gateway intent. `approximate_presence_count` is a field on the REST
--      response for GET /guilds/{id}?with_counts=true. It needs no
--      GuildPresences intent and src/discord/client.ts keeps its five.
--   2. It is a guild-level AGGREGATE. There is no per-member row anywhere in
--      it - not a member id, not a status. That makes it strictly
--      privacy-cheaper than the intent we declined, which would have streamed
--      a status change per member.
--   3. It is NEVER PUBLISHED. Not in `web_v1`, not behind a flag. The reason
--      the figure was rejected for the landing page still holds: it counts our
--      23 bots as people. That defect only matters to a stranger reading the
--      page - internally we know the bot floor, which is why it is a fine
--      instrument and a dishonest headline.
--
-- Containment is structural, not a promise. This table is in the BOT schema,
-- and src/store/webRole.ts REVOKEs the website role from every table in that
-- schema and grants SELECT only inside `web_v1`. So the website cannot read
-- this even if someone writes a view over it by mistake. Tests in
-- test/unit.presenceprobe.test.ts assert the file-level half of that.
--
-- Portable SQL on purpose: the other Postgres-only migrations are untestable
-- without a server, and the containment tests are the point of this issue. The
-- unit test applies THIS FILE to an in-memory SQLite database, so the schema
-- that ships is the schema the tests exercise.

CREATE TABLE IF NOT EXISTS presence_probe (
  guild_id                   TEXT NOT NULL,
  -- ISO-8601 UTC, matching every other timestamp in this codebase.
  observed_at                TEXT NOT NULL,

  -- Straight from Discord. Bots included - that is the known defect, and it is
  -- why nothing may render this without subtracting a floor it has verified.
  approximate_presence_count INTEGER NOT NULL,

  -- Members with `user.bot` true at the time of the reading. 23 on 19-Aug and
  -- it will drift, which is exactly why we record it rather than hardcode it.
  --
  -- NULLABLE, and NULL is the common case. The bot roster changes a few times
  -- a year; re-listing every member every hour would touch a lot of per-member
  -- data to re-derive a number that did not move. So the collector rescans at
  -- most once a day and writes NULL in between. A reader wanting the floor for
  -- a given reading takes the most recent non-NULL at or before it.
  bot_floor                  INTEGER,

  PRIMARY KEY (guild_id, observed_at),

  CONSTRAINT presence_probe_count_nonneg
    CHECK (approximate_presence_count >= 0),
  CONSTRAINT presence_probe_floor_nonneg
    CHECK (bot_floor IS NULL OR bot_floor >= 0)
);

-- There is deliberately NO `human_estimate` column, and adding one is not a
-- small change.
--
-- We cannot compute one honestly: `approximate_presence_count` is Discord's
-- own approximation, the bot floor is a count of bot ACCOUNTS rather than of
-- bots currently online, and subtracting the second from the first produces a
-- number with no defined meaning. A stored column named `human_estimate`
-- would nonetheless be trusted and eventually published by someone who read
-- the name and not this comment. Keep the guess in the reporting script
-- (scripts/presence-trend.ts), where it is visibly a derivation and carries
-- its own caveat, and out of the database, where it would look like a fact.

-- The only access pattern: one guild, newest first.
CREATE INDEX IF NOT EXISTS idx_presence_probe_observed
  ON presence_probe (guild_id, observed_at);
