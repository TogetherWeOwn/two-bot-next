-- 0471_voice_heat_hours: guild-wide voice occupancy samples per UTC hour.
--
-- Durable half of the voice-heat pipeline (contract v2 B5): the sampler
-- records `total_in_voice` (AFK- and bot-excluded, the `voice.snapshot`
-- definition) every five minutes into an in-memory accumulator and flushes
-- completed UTC hours here as `{hour_start_utc, samples, occupancy_sum, last_observed_at}`.
-- Aggregate-only by construction: no member id, channel id or per-room
-- split is ever recorded. A stopped gateway records nothing, so gaps stay
-- gaps, never zeros. Retention prunes rows older than 35 days; the
-- `web_v1.voice_heat` view publishes the trailing 28 complete days.
--
-- The `web_contract_meta.contract_version` 1.0 -> 1.1 bump for the new
-- `web_v1.voice_heat` view is NOT done here: it is published atomically at
-- the end of `web_v1.sql`, after the views install, so a migration alone never
-- claims views that do not exist yet.
-- Additive only: one new table, no changes to existing tables. `IF NOT EXISTS` throughout, matching the S6 convention.
-- Rollback is `DROP TABLE IF EXISTS voice_heat_hours`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS voice_heat_hours (
  hour_start_utc TIMESTAMPTZ PRIMARY KEY,
  samples        INTEGER NOT NULL DEFAULT 0,
  occupancy_sum  BIGINT NOT NULL DEFAULT 0,
  last_observed_at TIMESTAMPTZ,
  CONSTRAINT voice_heat_samples_bounded CHECK (samples BETWEEN 0 AND 12),
  CONSTRAINT voice_heat_sum_nonneg CHECK (occupancy_sum >= 0),
  CONSTRAINT voice_heat_observation_valid CHECK (
    (samples = 0 AND occupancy_sum = 0 AND last_observed_at IS NULL)
    OR (samples > 0 AND last_observed_at IS NOT NULL
        AND last_observed_at >= hour_start_utc
        AND last_observed_at < hour_start_utc + interval '1 hour')
  )
);
