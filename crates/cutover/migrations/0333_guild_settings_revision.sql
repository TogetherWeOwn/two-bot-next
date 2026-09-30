-- Preserve applied 0330–0332 checksums and existing row tokens. An installed
-- 0331 writer could have allocated from a shadow sequence before 0332 bound
-- the row trigger to its target schema. Reseed above those extant tokens and
-- unused canonical allocations before allowing any more settings DML.
-- The migration runner holds this lock until commit. Do not take the revision
-- lock here: a store writer may already hold it while waiting on this DDL.
LOCK TABLE guild_settings IN SHARE ROW EXCLUSIVE MODE;

DO $$
DECLARE
  target_schema TEXT;
  stored_max BIGINT;
  allocated_max BIGINT;
BEGIN
  SELECT n.nspname INTO STRICT target_schema
    FROM pg_catalog.pg_class c
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    WHERE c.oid = 'guild_settings'::pg_catalog.regclass;
  EXECUTE pg_catalog.format(
    'SELECT COALESCE(max(version), 0) FROM %I.guild_settings', target_schema
  ) INTO stored_max;
  EXECUTE pg_catalog.format(
    'SELECT last_value FROM %I.guild_settings_version_seq', target_schema
  ) INTO allocated_max;
  PERFORM pg_catalog.setval(
    pg_catalog.format('%I.%I', target_schema, 'guild_settings_version_seq')::pg_catalog.regclass,
    GREATEST(stored_max, allocated_max), true
  );
END;
$$;

-- Qualified direct writers must lock/advance the same revision as the store,
-- even with a shadow revision table first in their search_path. EXECUTE does
-- not set FOUND, so explicitly check its row count to keep the missing-row
-- guard fail-closed. The existing statement trigger uses this replacement.
CREATE OR REPLACE FUNCTION guild_settings_advance_revision() RETURNS TRIGGER AS $$
DECLARE
  affected BIGINT;
BEGIN
  EXECUTE pg_catalog.format(
    'UPDATE %I.guild_settings_revision SET revision = revision + 1 WHERE singleton = TRUE',
    TG_TABLE_SCHEMA
  );
  GET DIAGNOSTICS affected = ROW_COUNT;
  IF affected <> 1 THEN
    RAISE EXCEPTION 'guild_settings revision row is missing';
  END IF;
  RETURN NULL;
END;
$$ LANGUAGE plpgsql;
