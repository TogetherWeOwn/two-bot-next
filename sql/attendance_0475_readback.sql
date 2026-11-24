-- Run with psql -XqAt on the isolated copy. See docs/backup.md.
\set ON_ERROR_STOP on
\set VERBOSITY sqlstate
\if :{?expected_legacy}
\else
  \set expected_legacy ''
\endif
\if :{?expected_fence}
\else
  \set expected_fence ''
\endif

BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
SET LOCAL search_path = pg_catalog, pg_temp;
SET LOCAL statement_timeout = '15s';
SET LOCAL lock_timeout = '2s';
SET LOCAL row_security = off;
SET LOCAL timezone = 'UTC';
SET LOCAL datestyle = 'ISO, YMD';

SELECT current_database()::text = :'expected_database'
   AND current_user::text = :'expected_role' AND session_user = current_user
   AND NOT (r.rolsuper OR r.rolbypassrls OR r.rolcreaterole
            OR r.rolcreatedb OR r.rolreplication)
   AND NOT EXISTS (
       SELECT FROM pg_roles elevated
       WHERE elevated.rolname = 'two_bot_migrator'
         AND pg_has_role(r.oid, elevated.oid, 'MEMBER')
   )
   AND CASE :'phase'
       WHEN 'capture' THEN :'expected_legacy' = '' AND :'expected_fence' = ''
       WHEN 'before' THEN :'expected_legacy' ~ '^[0-9a-f]{64}$' AND :'expected_fence' = ''
       WHEN 'after' THEN :'expected_legacy' ~ '^[0-9a-f]{64}$'
                     AND :'expected_fence' ~ '^[0-9a-f]{64}$'
       ELSE false
   END AS binding_ok
FROM pg_roles r WHERE r.rolname = current_user
\gset
\if :binding_ok
\else
  \echo refused_binding
  SELECT 1 / 0; -- ON_ERROR_STOP exits nonzero on all supported psql versions.
\endif

SELECT COALESCE((
    SELECT c.relkind = 'r' AND NOT c.relrowsecurity AND NOT c.relhassubclass
       AND CASE WHEN :'phase' = 'after' THEN
           a.atttypid = 'timestamptz'::regtype AND NOT a.attnotnull
           AND pg_get_expr(d.adbin, d.adrelid) = 'CURRENT_TIMESTAMP'
       ELSE a.attnum IS NULL END
    FROM pg_class c
    LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND NOT a.attisdropped
                            AND a.attname = 'attendance_recovered_at'
    LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum
    WHERE c.oid = to_regclass('public.event_instance_windows')
), false) AS schema_ok
\gset
\if :schema_ok
\else
  \echo refused_schema
  SELECT 1 / 0;
\endif

-- Hash the original fields in primary-key order. Each input to string_agg is
-- fixed width. The extra row makes oversize input a refusal, never a sample.
-- The new column is optional in the query so the same projection works before
-- apply. The catalog check above decides when the column must exist.
WITH windows AS (
    SELECT w.guild_id, w.event_id, w.instance_starts_at,
           sha256(convert_to(jsonb_build_array(
               w.guild_id, w.event_id, w.instance_starts_at, w.starts_at, w.ends_at,
               w.status, w.observed_at, w.omitted_at, w.revoked_at,
               w.is_current, w.identity_ambiguous
           )::text, 'UTF8')) AS legacy,
           sha256(timestamptz_send((to_jsonb(w) ->> 'attendance_recovered_at')::timestamptz)) AS fence
    FROM public.event_instance_windows w LIMIT 100001
), totals AS (
    SELECT count(*) AS rows, count(fence) AS fenced,
           count(*) FILTER (WHERE encode(fence, 'hex') = :'expected_fence') AS matching_fences,
           encode(sha256(COALESCE(string_agg(legacy, ''::bytea ORDER BY
               guild_id COLLATE "C", event_id COLLATE "C", instance_starts_at), ''::bytea)), 'hex') AS legacy_sha,
           encode(sha256(COALESCE(string_agg(COALESCE(fence, sha256(''::bytea)),
               ''::bytea ORDER BY guild_id COLLATE "C", event_id COLLATE "C", instance_starts_at),
               ''::bytea)), 'hex') AS fence_sha
    FROM windows
)
SELECT CASE WHEN rows = 0 THEN 'empty' ELSE 'ok' END AS status,
       rows, legacy_sha, fenced, fence_sha,
       rows <= 100000
       AND (:'phase' = 'capture' OR legacy_sha = :'expected_legacy')
       AND (:'phase' <> 'after' OR matching_fences = rows) AS data_ok
FROM totals
\gset
\if :data_ok
\else
  \echo refused_data
  SELECT 1 / 0;
\endif

ROLLBACK;
\echo :status :rows :legacy_sha :fenced :fence_sha
