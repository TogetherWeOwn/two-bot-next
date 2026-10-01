-- Keep the earlier migration checksums and existing row tokens intact.
-- A qualified direct SQL writer can have a different search_path, including
-- a same-named sequence in an earlier schema. Allocate from the target table's
-- schema instead; never from caller lookup order or a hardcoded public schema.
CREATE OR REPLACE FUNCTION guild_settings_assign_version() RETURNS TRIGGER AS $$
BEGIN
  NEW.version := pg_catalog.nextval(
    pg_catalog.format('%I.%I', TG_TABLE_SCHEMA, 'guild_settings_version_seq')::pg_catalog.regclass
  );
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;
