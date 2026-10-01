-- Keep leveling XP inside JavaScript's safe integer range. The bot reads these
-- values as numbers, so larger BIGINTs cannot be represented exactly.
ALTER TABLE member_levels
  ADD CONSTRAINT member_levels_xp_js_safe
    CHECK (xp BETWEEN 0 AND 9007199254740991),
  ADD CONSTRAINT member_levels_message_xp_js_safe
    CHECK (message_xp BETWEEN 0 AND 9007199254740991),
  ADD CONSTRAINT member_levels_voice_xp_js_safe
    CHECK (voice_xp BETWEEN 0 AND 9007199254740991),
  ADD CONSTRAINT member_levels_imported_xp_js_safe
    CHECK (imported_xp BETWEEN 0 AND 9007199254740991);

ALTER TABLE level_import_runs
  ADD CONSTRAINT level_import_runs_total_imported_xp_js_safe
    CHECK (total_imported_xp BETWEEN 0 AND 9007199254740991);
