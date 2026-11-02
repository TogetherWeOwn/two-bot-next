-- 0457_admin_api_tokens: store-chain mirror of the admin token registry.
-- Renumbered before first merge: main claimed 0452/0453 for scheduled-event
-- tombstones, then 0454 for sticky delivery uncertainty during review.
-- Never merged under the old numbers, so no deployment applied them.
--
-- Same `admin_api_tokens` table as cutover 0456 (token id, sha256 secret
-- hash, scopes, guild allowlist, bounded lifetime, revocation/last-use
-- lifecycle columns). The store chain alone never reaches
-- staging/production; this mirror keeps single-chain checkouts and the
-- roles scratch database on the same shape. `IF NOT EXISTS` throughout, so
-- cross-running both chains against one schema stays safe in either order.
-- Rollback is `DROP TABLE IF EXISTS admin_api_tokens`. Numbered 0457
-- because cutover owns 0456; global migration numbers remain unique across
-- both chains. The live-lookup index builds inside the migration
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
