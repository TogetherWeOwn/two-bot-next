-- Read-only effective privilege inspection. One row per drift finding.
-- OIDs (not role-name privilege calls) make missing roles reportable, not errors.
WITH
expected_objects AS (
-- @matrix
),
expected_roles(name) AS (
    VALUES ('two_bot_migrator'), ('two_bot_runtime'), ('two_web_reader'), ('two_bot_migrator_ro')
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
objects AS (
    SELECT e.*, CASE WHEN e.kind = 'function'
        THEN to_regprocedure(e.schema_name || '.' || e.name)::oid
        ELSE to_regclass(format('%I.%I', e.schema_name, e.name))::oid END AS oid
    FROM expected_objects e
),
sequences AS (
    SELECT oid FROM objects WHERE kind = 'sequence'
    UNION
    SELECT c.oid FROM relations c JOIN pg_depend d ON d.objid = c.oid
        AND d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass
    JOIN objects o ON d.refobjid = o.oid AND o.kind = 'table'
    WHERE c.relkind = 'S' AND d.deptype IN ('a', 'i')
),
-- Admission objects, including durable voice-create reservations, preserve
-- SELECT/INSERT/UPDATE while never granting runtime DELETE.
table_grants(role_name, oid, privilege) AS (
    SELECT 'two_bot_runtime', o.oid, p.name FROM objects o
    CROSS JOIN (VALUES ('SELECT'), ('INSERT'), ('UPDATE'), ('DELETE')) p(name)
    WHERE o.kind = 'table' OR (o.kind = 'admission' AND p.name <> 'DELETE')
    UNION ALL
    SELECT 'two_web_reader', oid, 'SELECT' FROM objects WHERE kind = 'view'
    UNION ALL
    SELECT 'two_bot_migrator_ro', oid, 'SELECT' FROM objects
    WHERE kind IN ('table', 'admission', 'ledger')
    UNION ALL
    SELECT 'two_bot_migrator', o.oid, p.name FROM objects o
    CROSS JOIN (VALUES ('SELECT'), ('INSERT'), ('UPDATE'), ('DELETE'), ('TRUNCATE'), ('REFERENCES'), ('TRIGGER')) p(name)
    WHERE o.kind IN ('table', 'admission', 'ledger', 'view', 'migrator')
),
-- System objects have ordinary PUBLIC catalog access. Compare additional grants
-- with initdb's immutable PUBLIC baseline, not the possibly drifted current ACL.
-- Membership/attribute findings separately reject all inherited/bypass access.
-- Column baselines include table-wide rights; redundant non-grantable access is safe.
-- information_schema is installed after initdb records pg_init_privs. Its shipped
-- objects (OID < FirstNormalObjectId = 16384) allow SELECT and schema USAGE only.
-- User-added objects in that namespace do not inherit this exception.
information_schema_initial AS (
    SELECT c.oid, ARRAY[makeaclitem(0, c.relowner, 'SELECT', false)] AS acl
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'information_schema' AND c.oid < 16384 AND c.relkind IN ('r', 'v')
),
system_acls AS (
    SELECT c.oid::regclass::text AS target,
        coalesce(c.relacl, acldefault(CASE WHEN c.relkind = 'S' THEN 's'::"char" ELSE 'r'::"char" END, c.relowner)) AS acl,
        coalesce(i.initprivs, (SELECT acl FROM information_schema_initial WHERE oid = c.oid),
            acldefault(CASE WHEN c.relkind = 'S' THEN 's'::"char" ELSE 'r'::"char" END, c.relowner)) AS initial_acl
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    LEFT JOIN pg_init_privs i ON i.objoid = c.oid AND i.classoid = 'pg_class'::regclass AND i.objsubid = 0 AND i.privtype = 'i'
    WHERE NOT EXISTS (SELECT FROM app_schemas a WHERE a.oid = n.oid) AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
    UNION ALL
    SELECT c.oid::regclass::text || '.' || a.attname, a.attacl,
        coalesce(i.initprivs, (SELECT acl FROM information_schema_initial WHERE oid = c.oid),
            acldefault('r', c.relowner)) || coalesce(ci.initprivs, ARRAY[]::aclitem[])
    FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid
    LEFT JOIN pg_init_privs i ON i.objoid = c.oid AND i.classoid = 'pg_class'::regclass AND i.objsubid = 0 AND i.privtype = 'i'
    LEFT JOIN pg_init_privs ci ON ci.objoid = c.oid AND ci.classoid = 'pg_class'::regclass AND ci.objsubid = a.attnum AND ci.privtype = 'i'
    WHERE NOT EXISTS (SELECT FROM app_schemas n WHERE n.oid = c.relnamespace) AND a.attnum > 0 AND NOT a.attisdropped
    UNION ALL
    SELECT p.oid::regprocedure::text, coalesce(p.proacl, acldefault('f', p.proowner)),
        coalesce(i.initprivs, acldefault('f', p.proowner))
    FROM pg_proc p LEFT JOIN pg_init_privs i ON i.objoid = p.oid AND i.classoid = 'pg_proc'::regclass AND i.objsubid = 0 AND i.privtype = 'i'
    WHERE NOT EXISTS (SELECT FROM app_schemas n WHERE n.oid = p.pronamespace)
    UNION ALL
    SELECT n.nspname, coalesce(n.nspacl, acldefault('n', n.nspowner)),
        coalesce(i.initprivs, CASE WHEN n.nspname = 'information_schema' AND n.oid < 16384
            THEN ARRAY[makeaclitem(0, n.nspowner, 'USAGE', false)] ELSE acldefault('n', n.nspowner) END)
    FROM pg_namespace n LEFT JOIN pg_init_privs i ON i.objoid = n.oid AND i.classoid = 'pg_namespace'::regclass AND i.objsubid = 0 AND i.privtype = 'i'
    WHERE NOT EXISTS (SELECT FROM app_schemas a WHERE a.oid = n.oid)
),
system_owners AS (
    SELECT c.oid::regclass::text AS target, c.relowner AS owner,
        EXISTS (
            SELECT FROM objects o JOIN pg_class base ON base.oid = o.oid
            WHERE o.kind IN ('table', 'admission', 'ledger', 'migrator') AND base.reltoastrelid <> 0
              AND (c.oid = base.reltoastrelid OR EXISTS (
                  SELECT FROM pg_index i WHERE i.indexrelid = c.oid AND i.indrelid = base.reltoastrelid
              ))
        ) AS bot_storage
    FROM pg_class c WHERE NOT EXISTS (SELECT FROM app_schemas n WHERE n.oid = c.relnamespace)
    UNION ALL
    SELECT p.oid::regprocedure::text, p.proowner, false FROM pg_proc p
    WHERE NOT EXISTS (SELECT FROM app_schemas n WHERE n.oid = p.pronamespace)
    UNION ALL
    SELECT n.nspname, n.nspowner, false FROM pg_namespace n
    WHERE NOT EXISTS (SELECT FROM app_schemas a WHERE a.oid = n.oid)
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
    -- PostgreSQL 15+ parameter ACLs are cluster-wide, including PUBLIC grants.
    -- No group may acquire SET/ALTER SYSTEM through an explicit parameter ACL.
    SELECT 'unexpected parameter privilege: ' || r.rolname || '/' || p.parname || '/' || x.privilege_type
    FROM pg_parameter_acl p CROSS JOIN LATERAL aclexplode(p.paracl) x
    JOIN roles r ON x.grantee IN (0, r.oid)
    UNION ALL
    SELECT 'unexpected system owner: ' || r.rolname || '/' || o.target
    FROM system_owners o JOIN roles r ON r.oid = o.owner
    -- ALTER TABLE OWNER also transfers its TOAST table/index; those are the
    -- migrator's allowlisted table storage, not authority over catalog objects.
    WHERE NOT (r.rolname = 'two_bot_migrator' AND o.bot_storage)
    UNION ALL
    SELECT 'unexpected system privilege: ' || r.rolname || '/' || a.target || '/' || x.privilege_type
    FROM system_acls a CROSS JOIN LATERAL aclexplode(a.acl) x JOIN roles r ON x.grantee IN (0, r.oid)
    WHERE x.is_grantable OR NOT EXISTS (
        SELECT FROM aclexplode(a.initial_acl) baseline
        WHERE baseline.grantee = 0 AND baseline.privilege_type = x.privilege_type
    )
    UNION ALL
    SELECT 'missing CONNECT: ' || rolname FROM roles
    WHERE NOT has_database_privilege(oid, current_database(), 'CONNECT')
    UNION ALL
    SELECT 'unexpected database privilege: ' || r.rolname || '/' || p.name
    FROM roles r CROSS JOIN (VALUES ('CREATE'), ('TEMP')) p(name)
    WHERE r.rolname <> 'two_bot_migrator'
      AND has_database_privilege(r.oid, current_database(), p.name)
    UNION ALL
    SELECT 'unexpected migrator TEMP privilege' FROM roles
    WHERE rolname = 'two_bot_migrator' AND has_database_privilege(oid, current_database(), 'TEMP')
    UNION ALL
    SELECT 'missing database CREATE: two_bot_migrator' FROM roles
    WHERE rolname = 'two_bot_migrator'
      AND NOT has_database_privilege(oid, current_database(), 'CREATE')
    UNION ALL
    SELECT 'unexpected schema privilege: ' || r.rolname || '/' || n.nspname || '/' || p.name
    FROM roles r CROSS JOIN app_schemas n CROSS JOIN (VALUES ('USAGE'), ('CREATE')) p(name)
    WHERE r.rolname <> 'two_bot_migrator'
      AND NOT (p.name = 'USAGE' AND ((r.rolname = 'two_bot_runtime' AND n.nspname = 'public')
          OR (r.rolname = 'two_web_reader' AND n.nspname = 'web_v1')
          OR (r.rolname = 'two_bot_migrator_ro' AND n.nspname IN ('public', 'web_v1'))))
      AND has_schema_privilege(r.oid, n.oid, p.name)
    UNION ALL
    SELECT 'missing schema USAGE: ' || r.rolname || '/' || n.nspname
    FROM roles r CROSS JOIN (VALUES ('two_bot_runtime', 'public'), ('two_web_reader', 'web_v1'),
        ('two_bot_migrator_ro', 'public'), ('two_bot_migrator_ro', 'web_v1')) n(role_name, nspname)
    WHERE r.rolname = n.role_name
      AND NOT has_schema_privilege(r.oid, to_regnamespace(n.nspname), 'USAGE')
    UNION ALL
    SELECT 'missing schema: ' || e.name FROM (VALUES ('public'), ('web_v1')) e(name)
    WHERE to_regnamespace(e.name) IS NULL
    UNION ALL
    SELECT 'schema owner differs: ' || n.nspname FROM app_schemas n
    WHERE n.nspname IN ('public', 'web_v1')
      AND n.nspowner IS DISTINCT FROM (SELECT oid FROM roles WHERE rolname = 'two_bot_migrator')
    UNION ALL
    SELECT 'missing object: ' || schema_name || '.' || name FROM objects WHERE oid IS NULL
    UNION ALL
    SELECT 'object kind/owner differs: ' || o.schema_name || '.' || o.name
    FROM objects o JOIN relations c ON c.oid = o.oid WHERE o.kind <> 'function'
      AND (c.relowner IS DISTINCT FROM (SELECT oid FROM roles WHERE rolname = 'two_bot_migrator')
        OR NOT ((o.kind IN ('table', 'admission', 'ledger', 'migrator') AND c.relkind IN ('r', 'p'))
          OR (o.kind = 'view' AND c.relkind = 'v') OR (o.kind = 'sequence' AND c.relkind = 'S')))
    UNION ALL
    SELECT 'sequence owner differs: ' || c.oid::regclass::text FROM relations c JOIN sequences s ON s.oid = c.oid
    WHERE c.relowner IS DISTINCT FROM (SELECT oid FROM roles WHERE rolname = 'two_bot_migrator')
    UNION ALL
    SELECT 'missing table privilege: ' || g.role_name || '/' || g.oid::regclass::text || '/' || g.privilege
    FROM table_grants g JOIN roles r ON r.rolname = g.role_name
    WHERE NOT has_table_privilege(r.oid, g.oid, g.privilege)
    UNION ALL
    SELECT 'unexpected table privilege: ' || r.rolname || '/' || c.oid::regclass::text || '/' || p.name
    FROM roles r CROSS JOIN relations c
    CROSS JOIN (VALUES ('SELECT'), ('INSERT'), ('UPDATE'), ('DELETE'), ('TRUNCATE'), ('REFERENCES'), ('TRIGGER')) p(name)
    WHERE r.rolname <> 'two_bot_migrator' AND c.relkind <> 'S'
      AND NOT EXISTS (SELECT FROM table_grants g WHERE g.role_name = r.rolname AND g.oid = c.oid AND g.privilege = p.name)
      AND (has_table_privilege(r.oid, c.oid, p.name)
        OR (p.name IN ('SELECT', 'INSERT', 'UPDATE', 'REFERENCES') AND has_any_column_privilege(r.oid, c.oid, p.name)))
    UNION ALL
    -- Include privileges added by newer PostgreSQL releases (e.g. MAINTAIN)
    -- without calling has_table_privilege with a version-specific keyword.
    SELECT 'unexpected relation ACL: ' || r.rolname || '/' || c.oid::regclass::text || '/' || x.privilege_type
    FROM roles r CROSS JOIN relations c
    CROSS JOIN LATERAL aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) x
    WHERE r.rolname <> 'two_bot_migrator' AND c.relkind <> 'S'
      AND x.grantee IN (0, r.oid)
      AND NOT EXISTS (SELECT FROM table_grants g WHERE g.role_name = r.rolname AND g.oid = c.oid AND g.privilege = x.privilege_type)
    UNION ALL
    SELECT 'sequence privilege differs: ' || r.rolname || '/' || c.oid::regclass::text || '/' || p.name
    FROM roles r CROSS JOIN relations c CROSS JOIN (VALUES ('USAGE'), ('SELECT'), ('UPDATE')) p(name)
    WHERE r.rolname <> 'two_bot_migrator' AND c.relkind = 'S'
      AND has_sequence_privilege(r.oid, c.oid, p.name) IS DISTINCT FROM
        (r.rolname = 'two_bot_runtime' AND p.name IN ('USAGE', 'SELECT') AND EXISTS (SELECT FROM sequences s WHERE s.oid = c.oid))
    UNION ALL
    SELECT 'missing migrator sequence privilege: ' || c.oid::regclass::text || '/' || p.name
    FROM roles r CROSS JOIN sequences s JOIN relations c ON c.oid = s.oid
    CROSS JOIN (VALUES ('USAGE'), ('SELECT'), ('UPDATE')) p(name)
    WHERE r.rolname = 'two_bot_migrator' AND NOT has_sequence_privilege(r.oid, c.oid, p.name)
    UNION ALL
    SELECT 'missing function EXECUTE: ' || o.schema_name || '.' || o.name
    FROM objects o JOIN roles r ON r.rolname = 'two_bot_migrator'
    WHERE o.kind = 'function' AND NOT has_function_privilege(r.oid, o.oid, 'EXECUTE')
    UNION ALL
    SELECT 'function owner/security differs: ' || o.schema_name || '.' || o.name
    FROM objects o JOIN pg_proc p ON p.oid = o.oid WHERE o.kind = 'function'
      AND (p.proowner IS DISTINCT FROM (SELECT oid FROM roles WHERE rolname = 'two_bot_migrator') OR p.prosecdef)
    UNION ALL
    SELECT 'function privilege differs: ' || r.rolname || '/' || p.oid::regprocedure::text
    FROM roles r CROSS JOIN pg_proc p JOIN app_schemas n ON n.oid = p.pronamespace
    WHERE r.rolname <> 'two_bot_migrator'
      AND has_function_privilege(r.oid, p.oid, 'EXECUTE') IS DISTINCT FROM
        (r.rolname = 'two_web_reader' AND EXISTS (SELECT FROM objects o WHERE o.oid = p.oid AND o.kind = 'function' AND o.schema_name = 'web_v1'))
    UNION ALL
    SELECT 'security_invoker view: ' || c.oid::regclass::text FROM objects o JOIN relations c ON c.oid = o.oid
    WHERE o.kind = 'view' AND EXISTS (
        SELECT FROM pg_options_to_table(c.reloptions) option
        WHERE option.option_name = 'security_invoker' AND option.option_value::boolean
    )
    UNION ALL
    SELECT 'unexpected grant option: ' || r.rolname || '/' || a.target || '/' || x.privilege_type
    FROM (
        SELECT c.oid::regclass::text AS target, c.relacl AS acl FROM relations c
        UNION ALL SELECT c.oid::regclass::text || '.' || a.attname, a.attacl
            FROM relations c JOIN pg_attribute a ON a.attrelid = c.oid WHERE a.attnum > 0 AND NOT a.attisdropped
        UNION ALL SELECT n.nspname, n.nspacl FROM app_schemas n
        UNION ALL SELECT d.datname, d.datacl FROM pg_database d WHERE d.datname = current_database()
        UNION ALL SELECT p.oid::regprocedure::text, p.proacl FROM pg_proc p JOIN app_schemas n ON n.oid = p.pronamespace
    ) a CROSS JOIN LATERAL aclexplode(a.acl) x JOIN roles r ON r.oid = x.grantee
    WHERE r.rolname <> 'two_bot_migrator' AND x.is_grantable
    UNION ALL
    SELECT 'unsafe default grant: ' || pg_get_userbyid(d.defaclrole) || '/' || x.privilege_type
    FROM pg_default_acl d CROSS JOIN LATERAL aclexplode(d.defaclacl) x
    WHERE (x.grantee = 0 OR x.grantee IN (SELECT oid FROM roles WHERE rolname <> 'two_bot_migrator'))
      AND (d.defaclnamespace = 0 OR d.defaclnamespace IN (SELECT oid FROM app_schemas))
    UNION ALL
    SELECT 'migrator future functions executable by PUBLIC' FROM roles r
    CROSS JOIN LATERAL aclexplode(coalesce(
        (SELECT defaclacl FROM pg_default_acl WHERE defaclrole = r.oid AND defaclnamespace = 0 AND defaclobjtype = 'f'),
        acldefault('f', r.oid))) x
    WHERE r.rolname = 'two_bot_migrator' AND x.grantee = 0 AND x.privilege_type = 'EXECUTE'
)
SELECT finding FROM findings ORDER BY finding;
