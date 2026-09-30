-- Read-only effective privilege inspection. One row per drift finding.
-- OIDs (not role-name privilege calls) make missing roles reportable, not errors.
WITH
expected_roles(name) AS (
    VALUES ('two_bot_migrator'), ('two_bot_runtime'), ('two_web_reader')
),
roles AS (
    SELECT r.* FROM pg_roles r JOIN expected_roles e ON r.rolname = e.name
),
app_schemas AS (
    SELECT * FROM pg_namespace
    WHERE nspname NOT LIKE 'pg\_%' ESCAPE '\' AND nspname <> 'information_schema'
),
relations AS (
    SELECT c.*, n.nspname FROM pg_class c JOIN app_schemas n ON c.relnamespace = n.oid
    WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
),
findings AS (
    SELECT 'missing role: ' || e.name AS finding
    FROM expected_roles e LEFT JOIN roles r ON r.rolname = e.name WHERE r.oid IS NULL
    UNION ALL
    SELECT 'unsafe role attributes: ' || rolname FROM roles
    WHERE rolcanlogin OR rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls
    UNION ALL
    SELECT 'outgoing role membership: ' || r.rolname || ' -> ' || parent.rolname
    FROM roles r JOIN pg_auth_members m ON m.member = r.oid
    JOIN pg_roles parent ON parent.oid = m.roleid
    UNION ALL
    SELECT 'missing CONNECT: ' || rolname FROM roles
    WHERE NOT has_database_privilege(oid, current_database(), 'CONNECT')
    UNION ALL
    SELECT 'unexpected database privilege: ' || r.rolname || '/' || p.name
    FROM roles r CROSS JOIN (VALUES ('CREATE'), ('TEMP')) p(name)
    WHERE r.rolname <> 'two_bot_migrator'
      AND has_database_privilege(r.oid, current_database(), p.name)
    UNION ALL
    SELECT 'missing database CREATE: two_bot_migrator' FROM roles
    WHERE rolname = 'two_bot_migrator'
      AND NOT has_database_privilege(oid, current_database(), 'CREATE')
    UNION ALL
    SELECT 'unexpected schema privilege: ' || r.rolname || '/' || n.nspname || '/' || p.name
    FROM roles r CROSS JOIN app_schemas n CROSS JOIN (VALUES ('USAGE'), ('CREATE')) p(name)
    WHERE r.rolname <> 'two_bot_migrator'
      AND NOT (p.name = 'USAGE' AND ((r.rolname = 'two_bot_runtime' AND n.nspname = 'public')
          OR (r.rolname = 'two_web_reader' AND n.nspname = 'web_v1')))
      AND has_schema_privilege(r.oid, n.oid, p.name)
    UNION ALL
    SELECT 'missing schema USAGE: ' || r.rolname || '/' || n.nspname
    FROM roles r CROSS JOIN (VALUES ('two_bot_runtime', 'public'), ('two_web_reader', 'web_v1')) n(role_name, nspname)
    WHERE r.rolname = n.role_name
      AND NOT has_schema_privilege(r.oid, to_regnamespace(n.nspname), 'USAGE')
    UNION ALL
    SELECT 'missing schema: ' || e.name
    FROM (VALUES ('public'), ('web_v1')) e(name)
    WHERE to_regnamespace(e.name) IS NULL
    UNION ALL
    SELECT 'schema owner differs: ' || n.nspname FROM app_schemas n
    WHERE n.nspname IN ('public', 'web_v1')
      AND n.nspowner <> (SELECT oid FROM roles WHERE rolname = 'two_bot_migrator')
)
SELECT finding FROM findings ORDER BY finding;
