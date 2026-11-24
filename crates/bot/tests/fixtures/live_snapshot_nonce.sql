CREATE TABLE internal_nonces (
    nonce_hash text PRIMARY KEY CHECK (nonce_hash ~ '^[0-9a-f]{64}$'),
    burned_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    CHECK (expires_at >= burned_at + INTERVAL '241 seconds')
);

CREATE TABLE internal_clock_high_water (
    domain text PRIMARY KEY CHECK (domain ~ '^[a-z0-9_]{1,64}$'),
    high_water_ms bigint NOT NULL CHECK (high_water_ms >= 0),
    observed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK (observed_at >= TO_TIMESTAMP(high_water_ms::double precision / 1000.0))
);
