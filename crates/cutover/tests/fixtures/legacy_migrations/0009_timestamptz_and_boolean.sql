-- 0009_timestamptz_and_boolean: retire the two SQLite-isms 0001 carried over
-- on purpose (TOG-67, deferred from TWO-18).
--
--  * Every timestamp on the three funnel tables becomes timestamptz. The
--    stored TEXT was always ISO-8601 UTC written by toISOString(), so the
--    USING cast is exact; a malformed row - the thing this migration exists
--    to make impossible from here on - fails the transaction loudly instead
--    of converting.
--  * members.is_bot becomes BOOLEAN.
--
-- What keeps every reader working: the Postgres driver renders timestamptz
-- back as the same 'YYYY-MM-DDTHH24:MI:SS.MSZ' string the TEXT column held
-- (src/store/postgresDriver.ts), so JS-side string compares and slices are
-- untouched and scripts/funnel.ts output is byte-identical - that diff is in
-- the TOG-67 PR.
--
-- The 0002-0005 tables deliberately stay TEXT: their readers are string
-- comparisons that are correct for the format, none of them feeds the funnel,
-- and each conversion should carry its own before/after diff the way this one
-- does.
--
-- NOT additive (README rule 2 exception, called out in the PR): old code says
-- `is_bot = 0`, which has no operator against boolean. The bot applies this at
-- startup of the deploy that carries the new queries, so old queries never
-- meet the new schema in the bot's own process. The website reads only views,
-- and the same boot recreates those moments later via applyWebContract() -
-- emitting the identical text columns, so the v1 contract does not retype.
--
-- The DO block below is why this file can stay static SQL: ALTER COLUMN TYPE
-- refuses while any view reads the column, and the views live in a schema
-- whose name depends on where the tables are (web_v1 in production,
-- <schema>_web_v1 in tests - src/store/webContract.ts). Dropping by catalogue
-- dependency instead of by name finds every dependent view wherever it is,
-- and finds nothing in a fresh test schema, where migrations run before any
-- view exists.
DO $$
DECLARE v record;
BEGIN
  FOR v IN
    SELECT DISTINCT vns.nspname AS schema_name, vc.relname AS view_name
      FROM pg_depend d
      JOIN pg_rewrite  rw  ON rw.oid = d.objid
      JOIN pg_class    vc  ON vc.oid = rw.ev_class AND vc.relkind = 'v'
      JOIN pg_namespace vns ON vns.oid = vc.relnamespace
      JOIN pg_class    src ON src.oid = d.refobjid
      JOIN pg_namespace sns ON sns.oid = src.relnamespace
     WHERE d.classid = 'pg_rewrite'::regclass
       AND d.refclassid = 'pg_class'::regclass
       AND src.relname IN ('events', 'members', 'invite_snapshots')
       AND sns.nspname = current_schema()
       AND vc.oid <> src.oid
  LOOP
    EXECUTE format('DROP VIEW IF EXISTS %I.%I CASCADE', v.schema_name, v.view_name);
  END LOOP;
END $$;

ALTER TABLE events
  ALTER COLUMN occurred_at TYPE timestamptz USING occurred_at::timestamptz,
  ALTER COLUMN recorded_at DROP DEFAULT,
  ALTER COLUMN recorded_at TYPE timestamptz USING recorded_at::timestamptz,
  -- Truncated to milliseconds so what Postgres stores is exactly what the
  -- driver renders - a microsecond tail would survive in the column but
  -- vanish from every string the application ever sees.
  ALTER COLUMN recorded_at SET DEFAULT date_trunc('milliseconds', now());

ALTER TABLE members
  ALTER COLUMN joined_at           TYPE timestamptz USING joined_at::timestamptz,
  ALTER COLUMN first_message_at    TYPE timestamptz USING first_message_at::timestamptz,
  ALTER COLUMN first_voice_at      TYPE timestamptz USING first_voice_at::timestamptz,
  ALTER COLUMN last_active_at      TYPE timestamptz USING last_active_at::timestamptz,
  ALTER COLUMN left_at             TYPE timestamptz USING left_at::timestamptz,
  ALTER COLUMN inactive_flagged_at TYPE timestamptz USING inactive_flagged_at::timestamptz,
  ALTER COLUMN gate_cleared_at     TYPE timestamptz USING gate_cleared_at::timestamptz,
  ALTER COLUMN third_message_at    TYPE timestamptz USING third_message_at::timestamptz,
  ALTER COLUMN is_bot DROP DEFAULT,
  ALTER COLUMN is_bot TYPE boolean USING is_bot <> 0,
  ALTER COLUMN is_bot SET DEFAULT FALSE;

ALTER TABLE invite_snapshots
  ALTER COLUMN updated_at TYPE timestamptz USING updated_at::timestamptz;
