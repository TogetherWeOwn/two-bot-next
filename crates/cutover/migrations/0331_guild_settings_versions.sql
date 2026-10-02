-- Optimistic tokens must change for direct SQL as well as SettingsStore writes.
-- Keep 0330 immutable for databases that have already applied its checksum.
-- The migration runner wraps this file in a transaction. Block settings DML
-- while seeding the sequence and installing the row trigger; do not acquire the
-- revision lock here (a store writer may already hold it while waiting on DDL).
LOCK TABLE guild_settings IN SHARE ROW EXCLUSIVE MODE;

-- Older direct writers could supply a version ahead of the sequence. Never
-- reuse an extant token or move the allocator backwards during this upgrade.
SELECT setval('guild_settings_version_seq', GREATEST(
  (SELECT COALESCE(max(version), 0) FROM guild_settings),
  (SELECT last_value FROM guild_settings_version_seq)
), true);

CREATE OR REPLACE FUNCTION guild_settings_assign_version() RETURNS TRIGGER AS $$
BEGIN
  -- Ignore caller-supplied versions, even unchanged/backwards ones. An upsert
  -- may allocate twice, but only uniqueness, not consecutive numbers, matters.
  NEW.version := nextval('guild_settings_version_seq');
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_guild_settings_version
  BEFORE INSERT OR UPDATE ON guild_settings
  FOR EACH ROW EXECUTE FUNCTION guild_settings_assign_version();
