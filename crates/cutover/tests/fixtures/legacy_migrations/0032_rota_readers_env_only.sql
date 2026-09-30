-- The explicit rota notice reader binding cannot be reassigned through dashboard settings.
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_rota_readers_env_only CHECK (key NOT IN (
    'TWO_ONBOARDING_ROTA_READER_IDS'
  ));
