-- Dormant, aggregate-only voice heat. Both Bot migration chains install this table.
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
