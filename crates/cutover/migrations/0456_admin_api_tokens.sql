-- 0456_admin_api_tokens: scoped-token registry for the admin API (slice 0).
-- Renumbered before first merge: main claimed 0452/0453 for scheduled-event
-- tombstones, then 0454 for sticky delivery uncertainty during review.
-- Never merged under the old numbers, so no deployment applied them.
--
-- `admin_api_tokens` is the authoritative registry behind container
-- authorization of admin API calls: one row per operator-issued token with
-- its sha256 secret hash (the secret itself is never stored), scope set,
-- guild allowlist, bounded lifetime and revocation/last-use lifecycle
-- columns. Slice 0 ships the table plus the pure scope/authorize predicates
-- (`InternalFlags::admin_token_authorizes`); no binary reads or writes this
-- table yet, so dropping it breaks nothing. `IF NOT EXISTS` throughout,
-- matching the S6 convention, so cross-running the store and cutover chains
-- against one schema stays safe in either order. Rollback is
-- `DROP TABLE IF EXISTS admin_api_tokens`; re-applying this file restores
-- the registry. The live-lookup index builds inside the migration
-- transaction (CONCURRENTLY cannot run there, and concurrent builds
-- deadlock CI's migrators); the table is empty at first apply. Bot schema
-- range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS admin_api_tokens (
  token_id      TEXT         PRIMARY KEY,
  secret_hash   TEXT         NOT NULL,
  scopes        TEXT[]       NOT NULL,
  guild_ids     BIGINT[]     NOT NULL,
  label         TEXT,
  issued_by     TEXT,
  issued_at     TIMESTAMPTZ  NOT NULL,
  expires_at    TIMESTAMPTZ  NOT NULL,
  revoked_at    TIMESTAMPTZ,
  last_used_at  TIMESTAMPTZ,
  CONSTRAINT admin_api_tokens_id_shape CHECK (token_id ~ '^adm_[0-9a-f]{16}$'),
  CONSTRAINT admin_api_tokens_hash_shape CHECK (secret_hash ~ '^[0-9a-f]{64}$'),
  CONSTRAINT admin_api_tokens_scopes_closed CHECK (scopes <@ ARRAY['read', 'moderate', 'admin']),
  CONSTRAINT admin_api_tokens_guilds_nonempty CHECK (
    cardinality(guild_ids) > 0 AND array_position(guild_ids, NULL) IS NULL
  ),
  CONSTRAINT admin_api_tokens_expiry_ordered CHECK (expires_at > issued_at)
);

CREATE INDEX IF NOT EXISTS idx_admin_api_tokens_live
  ON admin_api_tokens (revoked_at, expires_at);
