CREATE TABLE internal_nonces (
    nonce_hash text PRIMARY KEY,
    burned_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL
);

CREATE TABLE internal_clock_high_water (
    domain text PRIMARY KEY,
    high_water_ms bigint NOT NULL,
    observed_at timestamptz NOT NULL
);
