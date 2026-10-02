-- F8 fail-closed clock policy: persist the DB freshness high-water mark so a
-- restart or failover cannot move DB time backwards past burned nonces.
-- One row per clock domain (today only `internal_nonce_db`), updated by the
-- burn path under the same transaction as the nonce row it guards.
-- `observed_at` is evidence (the DB instant that advanced the mark), never a
-- freshness input: decisions compare epoch milliseconds against `high_water_ms`.
CREATE TABLE internal_clock_high_water (
    domain TEXT PRIMARY KEY CHECK (domain ~ '^[a-z0-9_]{1,64}$'),
    high_water_ms BIGINT NOT NULL CHECK (high_water_ms >= 0),
    observed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK (observed_at >= TO_TIMESTAMP(high_water_ms::double precision / 1000.0))
);
