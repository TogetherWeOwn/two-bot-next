-- 0441_game_activity_guild_aggregates: guild-level game-activity aggregates.
--
-- Durable half of the game-activity analytics pipeline (core
-- `game_activity` sessionize/aggregate, fed by gateway presence deltas and
-- voice spans). One row per (guild_id, game): summed play millis, summed
-- in-voice millis and session counts. Guild-keyed only: no member column, no
-- per-member row, no per-member reader and no public-surface view (the
-- presence-probe containment precedent). The only reader is an operator trend
-- report; plodding hourly flushes upsert additively, so a retried tick after
-- a drained buffer writes nothing and never double-counts.
--
-- `game` is TEXT NOT NULL with `''` meaning untitled (`None` in
-- `GuildGameAggregate`): Postgres treats NULL key parts as distinct, so a
-- nullable game would admit duplicate untitled rows per guild. The writer
-- maps `None` to `''` and the reader maps `''` back; `bounded()` never
-- yields `Some("")`, so the mapping is unambiguous.
--
-- Additive only: one new table, no changes to existing tables. `IF NOT
-- EXISTS` throughout, matching the S6 convention, so cross-running the store
-- and cutover chains against one schema stays safe in either order. Rollback
-- is `DROP TABLE IF EXISTS game_activity_guild_aggregates`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS game_activity_guild_aggregates (
  guild_id        TEXT NOT NULL,
  game            TEXT NOT NULL,
  total_play_ms   BIGINT NOT NULL DEFAULT 0,
  in_voice_ms     BIGINT NOT NULL DEFAULT 0,
  session_count   BIGINT NOT NULL DEFAULT 0,
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (guild_id, game),
  CONSTRAINT game_activity_play_nonneg CHECK (total_play_ms >= 0),
  CONSTRAINT game_activity_voice_nonneg CHECK (in_voice_ms >= 0),
  CONSTRAINT game_activity_sessions_nonneg CHECK (session_count >= 0),
  CONSTRAINT game_activity_voice_within_total CHECK (in_voice_ms <= total_play_ms)
);
