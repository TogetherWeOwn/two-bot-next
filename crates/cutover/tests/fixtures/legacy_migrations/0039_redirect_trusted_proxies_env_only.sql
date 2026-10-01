-- TOG-9924: the redirect's trusted-proxy allowlist cannot be changed through
-- dashboard settings.
--
-- TWO_REDIRECT_TRUSTED_PROXIES decides whose X-Forwarded-For the redirect
-- believes when picking its per-caller throttle bucket
-- (src/redirect/config.ts). A stored value could bless a spoofed header and
-- let a client pick its own bucket, so this stays with the network binds in
-- the environment, never in guild_settings. Follows the per-area pattern of
-- 0029/0031/0032/0034; test/unit.settingscatalog.test.ts fails if this list
-- and the catalog's env_only set ever diverge.
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_redirect_trusted_proxies_env_only CHECK (key NOT IN (
    'TWO_REDIRECT_TRUSTED_PROXIES'
  ));
