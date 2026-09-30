-- Retire the legacy TEXT timestamp / SMALLINT boolean representation.
-- Unlike the legacy migration, this new chain may encounter tables already
-- normalized by the cutover tools. Convert only columns that still need it.
-- Malformed legacy values fail the transaction; no data is silently repaired.
DO $$
DECLARE c record;
DECLARE v record;
BEGIN
  IF EXISTS (
    SELECT 1 FROM information_schema.columns
     WHERE table_schema = current_schema()
       AND table_name IN ('events', 'members', 'invite_snapshots')
       AND ((column_name LIKE '%\_at' ESCAPE '\' AND data_type = 'text')
         OR (column_name = 'is_bot' AND data_type <> 'boolean'))
  ) THEN
    -- Dependent views must be recreated after a type change. The bot applies
    -- the contract only after the entire migration chain succeeds.
    FOR v IN
      SELECT DISTINCT vns.nspname AS schema_name, vc.relname AS view_name
        FROM pg_depend d
        JOIN pg_rewrite rw ON rw.oid = d.objid
        JOIN pg_class vc ON vc.oid = rw.ev_class AND vc.relkind = 'v'
        JOIN pg_namespace vns ON vns.oid = vc.relnamespace
        JOIN pg_class src ON src.oid = d.refobjid
        JOIN pg_namespace sns ON sns.oid = src.relnamespace
       WHERE d.classid = 'pg_rewrite'::regclass
         AND d.refclassid = 'pg_class'::regclass
         AND src.relname IN ('events', 'members', 'invite_snapshots')
         AND sns.nspname = current_schema()
         AND vc.oid <> src.oid
    LOOP
      EXECUTE format('DROP VIEW IF EXISTS %I.%I CASCADE', v.schema_name, v.view_name);
    END LOOP;
  END IF;

  FOR c IN
    SELECT table_schema, table_name, column_name FROM information_schema.columns
     WHERE table_schema = current_schema()
       AND table_name IN ('events', 'members', 'invite_snapshots')
       AND column_name LIKE '%\_at' ESCAPE '\'
       AND data_type = 'text'
  LOOP
    IF c.table_name = 'events' AND c.column_name = 'recorded_at' THEN
      EXECUTE format('ALTER TABLE %I.events ALTER COLUMN recorded_at DROP DEFAULT', c.table_schema);
    END IF;
    EXECUTE format('ALTER TABLE %I.%I ALTER COLUMN %I TYPE timestamptz USING %I::timestamptz',
      c.table_schema, c.table_name, c.column_name, c.column_name);
  END LOOP;

  ALTER TABLE events ALTER COLUMN recorded_at SET DEFAULT date_trunc('milliseconds', now());
  IF EXISTS (
    SELECT 1 FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'members'
       AND column_name = 'is_bot' AND data_type <> 'boolean'
  ) THEN
    ALTER TABLE members ALTER COLUMN is_bot DROP DEFAULT;
    ALTER TABLE members ALTER COLUMN is_bot TYPE boolean USING is_bot <> 0;
    ALTER TABLE members ALTER COLUMN is_bot SET DEFAULT FALSE;
  END IF;
END $$;
