-- Guild-settings hot reload store (TOG-10096, S6 slice of TOG-9811).
--
-- Ports legacy two-bot `migrations/0026_guild_settings.sql` (the config store
-- behind the admin dashboard) plus every env-only refusal that followed it:
-- `0027_guild_settings_env_only.sql` (TOG-3183: TWO_MODERATION /
-- TWO_ONBOARDING_MODE co-gate capability from outside the TWO_INTERNAL_*
-- namespace, plus secrets, boot inputs and network binds),
-- `0029_onboarding_rota_env_only.sql` / `0031` / `0032` (rota collection,
-- primary identity and readers — the rota runtime is DROPped in two-bot-next
-- but the refusal stays: never dashboard-settable),
-- `0034_staging_restart_env_only.sql` (restart safety controls), and
-- `0039_redirect_trusted_proxies_env_only.sql` (TOG-9924: the throttle-bucket
-- allowlist could bless a spoofed X-Forwarded-For).
--
-- Legacy table and column names are kept verbatim. Env stays the permanent
-- fallback: a key with no row reads from the environment exactly as before,
-- so the day this ships nothing changes and the undo path is "stop writing
-- rows". Keyed per guild from day one.
--
-- Row versions retain the legacy sequence, but sequence allocation order is
-- not commit order. The poll reads a transactional singleton revision instead:
-- every settings statement takes that row lock before changing any setting,
-- advances the revision, and holds the lock until commit. Deletes advance it
-- too, even if another insert leaves the count unchanged between 15 s polls.
-- The store takes the same lock before reading the old value for its audit.

-- Block 0330–0339 is reserved for this card so parallel S6 slices never
-- collide; this is the only migration the slice needs.

CREATE SEQUENCE IF NOT EXISTS guild_settings_version_seq AS BIGINT START 1;

CREATE TABLE IF NOT EXISTS guild_settings (
  guild_id    TEXT NOT NULL,
  key         TEXT NOT NULL,
  value       JSONB NOT NULL,
  version     BIGINT NOT NULL,
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- Discord user id of the admin who saved it, or a named process for a
  -- machine write. Never null: an unattributed settings change is the one kind
  -- of row this table exists to make impossible.
  updated_by  TEXT NOT NULL,
  PRIMARY KEY (guild_id, key),
  -- The TWO_INTERNAL_* capability gates decide what the *website* may make
  -- the bot do. A settings write that could widen that allowlist is a
  -- privilege-escalation primitive. The application refuses these keys; the
  -- constraint is here as well so a bug, a psql session or a future writer
  -- cannot get past it (TOG-3093 ADR §2.4).
  CONSTRAINT guild_settings_no_internal_keys CHECK (key NOT LIKE 'TWO\_INTERNAL\_%')
);

CREATE INDEX IF NOT EXISTS idx_guild_settings_version ON guild_settings (version);

CREATE TABLE IF NOT EXISTS guild_settings_revision (
  singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton = TRUE),
  revision  BIGINT NOT NULL DEFAULT 0
);
INSERT INTO guild_settings_revision (singleton, revision) VALUES (TRUE, 0)
  ON CONFLICT (singleton) DO NOTHING;

CREATE OR REPLACE FUNCTION guild_settings_advance_revision() RETURNS TRIGGER AS $$
BEGIN
  UPDATE guild_settings_revision SET revision = revision + 1 WHERE singleton = TRUE;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'guild_settings revision row is missing';
  END IF;
  RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- BEFORE STATEMENT locks the revision before any settings row locks, including
-- direct SQL and inserts into absent keys. ON CONFLICT can advance it twice;
-- only change detection matters, not the number of allocated revisions.
DROP TRIGGER IF EXISTS trg_guild_settings_revision ON guild_settings;
CREATE TRIGGER trg_guild_settings_revision
  BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON guild_settings
  FOR EACH STATEMENT EXECUTE FUNCTION guild_settings_advance_revision();

-- Append-only. Who changed the bot's behaviour, when, and what it was before
-- is the record that matters after an incident, so it is not the writer's
-- choice whether to keep it. One audit row per change, including deletes
-- (NULL new_value = the row was deleted, handing the key back to env) and
-- including refused writes' absence (refusals happen before any SQL, so they
-- write no audit row either).
CREATE TABLE IF NOT EXISTS guild_settings_audit (
  id          BIGSERIAL PRIMARY KEY,
  guild_id    TEXT NOT NULL,
  key         TEXT NOT NULL,
  -- NULL old_value = the key had no row (it was reading from the environment).
  -- NULL new_value = the row was deleted.
  old_value   JSONB,
  new_value   JSONB,
  actor       TEXT NOT NULL,
  at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_guild_settings_audit_key
  ON guild_settings_audit (guild_id, key, at DESC);

-- Enforce append-only for every destructive statement, including TRUNCATE.
-- Tests reset their own disposable schemas, never runtime audit tables.
CREATE OR REPLACE FUNCTION guild_settings_audit_append_only() RETURNS TRIGGER AS $$
BEGIN
  RAISE EXCEPTION 'guild_settings_audit is append-only (attempted %)', TG_OP;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_guild_settings_audit_append_only ON guild_settings_audit;
CREATE TRIGGER trg_guild_settings_audit_append_only
  BEFORE UPDATE OR DELETE OR TRUNCATE ON guild_settings_audit
  FOR EACH STATEMENT EXECUTE FUNCTION guild_settings_audit_append_only();

-- TOG-3183: the prefix is narrower than the set of keys that gate capability.
-- The application refuses all of these in two_bot_core::settings; these
-- constraints are the second half of the same rule. Names, not a prefix,
-- because these have nothing in common lexically — that is precisely why the
-- prefix missed them.
ALTER TABLE guild_settings
  DROP CONSTRAINT IF EXISTS guild_settings_env_only_keys;
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_env_only_keys CHECK (key NOT IN (
    'DISCORD_BOT_TOKEN',
    'DISCORD_TOKEN',
    'TWO_MODERATION_AUDIT_SECRET',
    'TWO_DATABASE_URL',
    'TWO_STAGING_DATABASE_URL',
    'DISCORD_STAGING_BOT_TOKEN',
    'TWO_BACKUP_S3_ACCESS_KEY_ID',
    'TWO_BACKUP_S3_SECRET_ACCESS_KEY',
    'TWO_BACKUP_S3_BUCKET',
    'TWO_BACKUP_S3_ENDPOINT',
    'TWO_BACKUP_S3_PREFIX',
    'TWO_BACKUP_S3_REGION',
    'CREDENTIALS_DIRECTORY',
    'TWO_DB_POOL_MAX',
    'DISCORD_GUILD_ID',
    'DISCORD_STAGING_GUILD_ID',
    'TWO_HEALTH_BIND_HOST',
    'TWO_HEALTH_PORT',
    'TWO_REDIRECT_BIND_HOST',
    'TWO_REDIRECT_PORT',
    'DISCORD_API_BASE',
    'TWO_MODERATION',
    'TWO_ONBOARDING_MODE',
    'TWO_MODERATION_PROTECTED_ROLE_IDS',
    'TWO_OWEN_USER_ID',
    'TWO_ANTI_NUKE_PROTECTED_USER_IDS',
    'TWO_ANTI_NUKE_TRUSTED_USER_IDS',
    'TWO_ANTI_NUKE_SNAPSHOT_PATH'
  ));

-- Rota collection/notice gates, primary identity, and readers: never
-- dashboard-settable (TOG-3531 + follow-ups).
ALTER TABLE guild_settings
  DROP CONSTRAINT IF EXISTS guild_settings_rota_env_only_keys;
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_rota_env_only_keys CHECK (key NOT IN (
    'TWO_ONBOARDING_ROTA_MEASUREMENT',
    'TWO_ONBOARDING_ROTA_NOTICE',
    'TWO_ONBOARDING_ROTA_PSEUDONYM_KEY'
  ));
ALTER TABLE guild_settings
  DROP CONSTRAINT IF EXISTS guild_settings_rota_primary_env_only;
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_rota_primary_env_only CHECK (key NOT IN (
    'TWO_ONBOARDING_ROTA_PRIMARY_ACTOR_ID'
  ));
ALTER TABLE guild_settings
  DROP CONSTRAINT IF EXISTS guild_settings_rota_readers_env_only;
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_rota_readers_env_only CHECK (key NOT IN (
    'TWO_ONBOARDING_ROTA_READER_IDS'
  ));

-- Staging restart safety controls (legacy 0034).
ALTER TABLE guild_settings
  DROP CONSTRAINT IF EXISTS guild_settings_staging_restart_env_only;
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_staging_restart_env_only CHECK (key NOT IN (
    'TWO_STAGING_RESTART_CONTAINMENT',
    'TWO_STAGING_RESTART_SYNTHETIC_ACTORS'
  ));

-- Redirect trusted-proxy allowlist (legacy 0039, TOG-9924).
ALTER TABLE guild_settings
  DROP CONSTRAINT IF EXISTS guild_settings_redirect_trusted_proxies_env_only;
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_redirect_trusted_proxies_env_only CHECK (key NOT IN (
    'TWO_REDIRECT_TRUSTED_PROXIES'
  ));
