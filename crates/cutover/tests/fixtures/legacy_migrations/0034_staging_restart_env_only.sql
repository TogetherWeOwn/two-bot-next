-- Staging restart safety controls cannot be changed through dashboard settings.
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_staging_restart_env_only CHECK (key NOT IN (
    'TWO_STAGING_RESTART_CONTAINMENT',
    'TWO_STAGING_RESTART_SYNTHETIC_ACTORS'
  ));
