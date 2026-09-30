-- TOG-3531: the staging-only collection/notice gates and HMAC key are not
-- dashboard settings. Keep 0027's existing refusal intact and add this one.
-- 0028 is reserved for the separately reviewed onboarding measurement core.
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_rota_env_only_keys CHECK (key NOT IN (
    'TWO_ONBOARDING_ROTA_MEASUREMENT',
    'TWO_ONBOARDING_ROTA_NOTICE',
    'TWO_ONBOARDING_ROTA_PSEUDONYM_KEY'
  ));
