-- Participation-rewards anti-gaming (TOG-22973, AC2/AC3).
--
-- Seasonal projection (`member_season_xp` parallels `member_levels` per
-- quarter), the policy `decision` audit marker on `xp_awards`, and widened
-- audit CHECKs so capped attempts can leave 0-XP rows and the decay job can
-- write `source='decay'` rows.
--
-- Additive + nullable only: no existing column is altered, renamed or
-- dropped, and every new column is nullable or lives on the new table. The
-- two CHECKs below are the sole exception, and only by widening: each
-- previously valid row still validates (`xp > 0` implies `xp >= 0`;
-- `IN (...)` gains one member). Dropped with IF EXISTS guards because
-- Postgres auto-names inline CHECKs; re-added idempotently like the rest of
-- this chain (`IF NOT EXISTS` everywhere a name is introduced).

-- Seasonal projection: append-only mirror of lifetime XP per quarter.
-- `updated_at` tracks the last award so the decay job can age rows; decay
-- updates `seasonal_xp` only and never touches `member_levels`.
CREATE TABLE IF NOT EXISTS member_season_xp (
  guild_id    TEXT        NOT NULL,
  season_id   TEXT        NOT NULL,
  member_id   TEXT        NOT NULL,
  seasonal_xp BIGINT      NOT NULL CHECK (seasonal_xp >= 0),
  updated_at  timestamptz NOT NULL,
  PRIMARY KEY (guild_id, season_id, member_id),
  CHECK (seasonal_xp BETWEEN 0 AND 9007199254740991)
);

CREATE INDEX IF NOT EXISTS idx_member_season_xp_member
  ON member_season_xp (guild_id, member_id, season_id);

-- Policy outcome marker: NULL on standard grants (written before this
-- slice), otherwise the anti-gaming decision (`daily_cap`,
-- `daily_ladder_exhausted`, `seasonal_decay`, ...). Capped attempts leave a
-- 0-XP audit row carrying the decision instead of vanishing like cooldown
-- rejections; cooldown rejections still write nothing.
ALTER TABLE xp_awards
  ADD COLUMN IF NOT EXISTS decision TEXT;

-- Widened audit CHECKs (see header): 0-XP capped rows and decay rows.
ALTER TABLE xp_awards DROP CONSTRAINT IF EXISTS xp_awards_xp_check;
ALTER TABLE xp_awards
  ADD CONSTRAINT xp_awards_xp_check CHECK (xp >= 0);
ALTER TABLE xp_awards DROP CONSTRAINT IF EXISTS xp_awards_source_check;
ALTER TABLE xp_awards
  ADD CONSTRAINT xp_awards_source_check CHECK (source IN ('message', 'voice', 'decay'));
