-- TOG-3100 / TOG-3183: widen the env-only refusal past the TWO_INTERNAL_ prefix.
--
-- 0026 refused `TWO\_INTERNAL\_%` and that constraint stays. The security review
-- of PR #125 showed the prefix is not the same set as "the keys that gate
-- capability": `TWO_MODERATION` co-gates nine moderation verbs with
-- `TWO_INTERNAL_ALLOW_MODERATION` at src/internal/config.ts:83 and carries no
-- prefix, and `TWO_ONBOARDING_MODE` is fed through actionsForOnboardingMode()
-- at src/index.ts:543, which decides whether `role.assign` is in the allowlist
-- at all. Neither was refused. Both were stored through the live handler in the
-- reviewer's probe, along with DISCORD_TOKEN and the database URL.
--
-- The application refuses all of these in src/core/settingsCatalog.ts. This
-- constraint is the second half of the same rule: a bug in the handler, a psql
-- session, or a writer nobody has written yet cannot get past the schema.
--
-- Names, not a prefix, because these have nothing in common lexically - that is
-- precisely why the prefix missed them. src/core/settingsCatalog.ts is the
-- source of truth and test/unit.settingscatalog.test.ts fails if this list and
-- the catalog's env_only-outside-the-prefix set ever diverge.

ALTER TABLE guild_settings
  DROP CONSTRAINT IF EXISTS guild_settings_env_only_keys;

ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_env_only_keys CHECK (key NOT IN (
    -- Secrets. The first two are the two the TOG-3100 census grep could not
    -- see, because they reach src/ as a readSecret() fallback array rather than
    -- as process.env.X - see SECRET_NAMES_NOT_IN_SRC_GREP.
    'DISCORD_BOT_TOKEN',
    'DISCORD_TOKEN',
    'TWO_MODERATION_AUDIT_SECRET',
    'TWO_DATABASE_URL',
    'TWO_STAGING_DATABASE_URL',
    'DISCORD_STAGING_BOT_TOKEN',
    'TWO_BACKUP_S3_ACCESS_KEY_ID',
    'TWO_BACKUP_S3_SECRET_ACCESS_KEY',

    -- Where a backup is shipped. Not a secret, but a settable endpoint turns
    -- the backup job into an exfiltration channel.
    'TWO_BACKUP_S3_BUCKET',
    'TWO_BACKUP_S3_ENDPOINT',
    'TWO_BACKUP_S3_PREFIX',
    'TWO_BACKUP_S3_REGION',

    -- Boot. Read before this table is reachable, or used to find it.
    'CREDENTIALS_DIRECTORY',
    'TWO_DB_POOL_MAX',
    'DISCORD_GUILD_ID',
    'DISCORD_STAGING_GUILD_ID',

    -- Network binds, and the Discord API host itself.
    'TWO_HEALTH_BIND_HOST',
    'TWO_HEALTH_PORT',
    'TWO_REDIRECT_BIND_HOST',
    'TWO_REDIRECT_PORT',
    'DISCORD_API_BASE',

    -- Capability gates outside the TWO_INTERNAL_ namespace. The finding.
    'TWO_MODERATION',
    'TWO_ONBOARDING_MODE',

    -- Not switches, but the bounds on verbs that are already switched on:
    -- who moderation may not touch, and who anti-nuke ignores
    -- (src/moderation/containment.ts:112-113).
    'TWO_MODERATION_PROTECTED_ROLE_IDS',
    'TWO_OWEN_USER_ID',
    'TWO_ANTI_NUKE_PROTECTED_USER_IDS',
    'TWO_ANTI_NUKE_TRUSTED_USER_IDS',

    -- A filesystem path chosen by a web form is a write primitive.
    'TWO_ANTI_NUKE_SNAPSHOT_PATH'
  ));
