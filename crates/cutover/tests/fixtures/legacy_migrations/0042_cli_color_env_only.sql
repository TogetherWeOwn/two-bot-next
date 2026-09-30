-- TOG-8698: terminal-output presentation keys cannot be changed through
-- dashboard settings.
--
-- NO_COLOR / FORCE_COLOR / TERM only decide whether CLI text carries ANSI
-- styling (src/analytics/cliColor.ts, read per render). A stored value would
-- let a dashboard write change operator-visible output with no restart and no
-- audit trail, so these stay with the process environment, never in
-- guild_settings. Follows the per-area pattern of 0029/0031/0032/0034/0039;
-- test/unit.settingscatalog.test.ts fails if this list and the catalog's
-- env_only set ever diverge.
ALTER TABLE guild_settings
  ADD CONSTRAINT guild_settings_cli_color_env_only CHECK (key NOT IN (
    'NO_COLOR',
    'FORCE_COLOR',
    'TERM'
  ));
