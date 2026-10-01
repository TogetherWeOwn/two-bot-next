-- The accepted primary identity cannot be reassigned through dashboard settings.
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_rota_primary_env_only CHECK (key NOT IN (
    'TWO_ONBOARDING_ROTA_PRIMARY_ACTOR_ID'
  ));
