-- 0472_voice_heat_hours: guild-wide voice occupancy samples per UTC hour.
--
-- Cutover mirror of the store 0471 voice-heat table (contract v2 B5): the
-- website-job sampler writes through the cutover pool, so the table must
-- exist in this chain too. Same `{hour_start_utc, samples, occupancy_sum, last_observed_at}`
-- shape, aggregate-only by construction (no member id, channel id or
-- per-room split), completed-hour-only flushes and 35-day retention. The
-- 1.0 -> 1.1 `web_contract_meta` bump for the new `web_v1.voice_heat` view is
-- not done here; it is published atomically at the end of `web_v1.sql`, after
-- the views install. Additive only; `IF NOT EXISTS` throughout so
-- cross-running both chains in either order converges. Rollback is
-- `DROP TABLE IF EXISTS voice_heat_hours`.
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
