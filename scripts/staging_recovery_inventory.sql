SET ROLE two_bot_migrator_ro;
BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
SET LOCAL search_path = pg_catalog, pg_temp;
SET LOCAL statement_timeout = '20s';
SET LOCAL lock_timeout = '5s';
SET LOCAL TIME ZONE 'UTC';

-- ON_ERROR_STOP refuses a misbound login before the inventory statement.
SELECT 1 / ((current_user = 'two_bot_migrator_ro'
    AND NOT pg_has_role(session_user, 'two_bot_migrator', 'MEMBER'))::integer) AS guard
\gset

WITH RECURSIVE expected AS (
    -- @matrix
), schemas AS (
    SELECT oid, nspname, nspowner, nspacl
    FROM pg_namespace WHERE nspname IN ('public', 'web_v1')
), relations AS (
    SELECT c.*, s.nspname AS schema_name
    FROM pg_class c JOIN schemas s ON s.oid = c.relnamespace
    WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f', 'c')
      AND (s.nspname = 'web_v1' OR pg_get_userbyid(c.relowner) = 'two_bot_migrator'
           OR EXISTS (SELECT 1 FROM expected e WHERE e.schema_name = s.nspname
                      AND e.name = c.relname AND e.kind <> 'function'))
), routines AS (
    SELECT p.*, s.nspname AS schema_name,
           format('%s(%s)', p.proname, oidvectortypes(p.proargtypes)) AS signature
    FROM pg_proc p JOIN schemas s ON s.oid = p.pronamespace
    WHERE s.nspname = 'web_v1' OR pg_get_userbyid(p.proowner) = 'two_bot_migrator'
       OR EXISTS (SELECT 1 FROM expected e WHERE e.schema_name = s.nspname
                  AND e.name = format('%s(%s)', p.proname, oidvectortypes(p.proargtypes))
                  AND e.kind = 'function')
), present AS (
    SELECT schema_name, relname::text AS name, CASE relkind
        WHEN 'S' THEN 'sequence' WHEN 'v' THEN 'view'
        WHEN 'r' THEN 'table' WHEN 'p' THEN 'table' END AS kind FROM relations
    UNION ALL SELECT schema_name, signature, 'function' FROM routines WHERE prokind = 'f'
), principals AS (
    SELECT oid FROM pg_roles WHERE rolname IN (session_user, 'two_bot_migrator',
        'two_bot_migrator_ro', 'two_bot_runtime', 'two_web_reader')
    UNION SELECT datdba FROM pg_database WHERE datname = current_database()
    UNION SELECT nspowner FROM schemas
    UNION SELECT relowner FROM relations
    UNION SELECT proowner FROM routines
    UNION SELECT d.refobjid FROM pg_shdepend d
    JOIN pg_database b ON b.datname = current_database()
    WHERE d.refclassid = 'pg_authid'::regclass AND (
        (d.dbid = 0 AND d.classid = 'pg_database'::regclass AND d.objid = b.oid)
        OR (d.dbid = b.oid AND CASE d.classid
            WHEN 'pg_class'::regclass THEN d.objid IN (SELECT oid FROM relations)
            WHEN 'pg_proc'::regclass THEN d.objid IN (SELECT oid FROM routines)
            WHEN 'pg_namespace'::regclass THEN d.objid IN (SELECT oid FROM schemas)
            ELSE false END)
    )
), scoped_roles AS (
    SELECT oid FROM principals
    UNION SELECT m.member FROM pg_auth_members m JOIN scoped_roles r ON m.roleid = r.oid
), role_groups AS (
    SELECT oid FROM scoped_roles
    UNION SELECT m.roleid FROM pg_auth_members m JOIN role_groups r ON m.member = r.oid
), app_defaults AS (
    SELECT d.*, s.nspname FROM pg_default_acl d LEFT JOIN schemas s ON s.oid = d.defaclnamespace
    WHERE (d.defaclnamespace = 0 OR s.oid IS NOT NULL)
      AND (d.defaclrole IN (SELECT oid FROM role_groups) OR EXISTS (
          SELECT 1 FROM aclexplode(d.defaclacl) a
          WHERE a.grantee = 0 OR a.grantee IN (SELECT oid FROM role_groups)))
), reported_roles AS (
    SELECT oid FROM role_groups
    UNION SELECT defaclrole FROM app_defaults
    UNION SELECT grantor FROM app_defaults CROSS JOIN LATERAL aclexplode(defaclacl)
    UNION SELECT grantee FROM app_defaults CROSS JOIN LATERAL aclexplode(defaclacl)
    UNION SELECT m.roleid FROM pg_auth_members m JOIN reported_roles r ON m.member = r.oid
)
SELECT jsonb_build_object(
    'observed_at', transaction_timestamp(),
    'server_version', current_setting('server_version'),
    'role', current_user,
    'database', (SELECT jsonb_build_object(
        'name', datname, 'owner', pg_get_userbyid(datdba), 'acl', datacl)
        FROM pg_database WHERE datname = current_database()),
    'schemas', ARRAY(SELECT jsonb_build_object(
        'name', nspname, 'owner', pg_get_userbyid(nspowner), 'acl', nspacl)
        FROM schemas ORDER BY nspname),
    'missing_expected', ARRAY(SELECT e.schema_name || '.' || e.name FROM expected e
        WHERE NOT EXISTS (SELECT 1 FROM present p
            WHERE p.schema_name = e.schema_name AND p.name = e.name
              AND p.kind = CASE WHEN e.kind IN ('sequence', 'view', 'function')
                                THEN e.kind ELSE 'table' END)
        ORDER BY e.schema_name, e.name),
    'unsupported', EXISTS (SELECT 1 FROM relations WHERE relkind IN ('f', 'c'))
        OR EXISTS (SELECT 1 FROM routines WHERE prokind = 'a')
        OR EXISTS (SELECT 1 FROM pg_shdepend d JOIN pg_database b ON b.oid = d.dbid
            WHERE b.datname = current_database() AND d.deptype = 'o'
              AND d.refobjid = 'two_bot_migrator'::regrole::oid
              AND d.classid NOT IN ('pg_class'::regclass, 'pg_proc'::regclass, 'pg_type'::regclass,
                  'pg_namespace'::regclass, 'pg_extension'::regclass, 'pg_default_acl'::regclass))
        OR EXISTS (SELECT 1 FROM pg_type t JOIN schemas s ON s.oid = t.typnamespace
            WHERE t.typrelid = 0 AND t.typelem = 0
              AND (s.nspname = 'web_v1' OR pg_get_userbyid(t.typowner) = 'two_bot_migrator')
              AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_type'::regclass
                              AND d.objid = t.oid AND d.deptype = 'e')),
    'extensions', ARRAY(SELECT jsonb_build_object(
        'name', e.extname, 'schema', n.nspname, 'version', e.extversion,
        'owner', pg_get_userbyid(e.extowner))
        FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace ORDER BY e.extname),
    'relations', ARRAY(SELECT jsonb_build_object(
        'schema', c.schema_name, 'name', c.relname, 'kind', c.relkind,
        'owner', pg_get_userbyid(c.relowner), 'acl', c.relacl,
        'options', c.reloptions, 'persistence', c.relpersistence,
        'tablespace', (SELECT spcname FROM pg_tablespace WHERE oid = c.reltablespace),
        'access_method', (SELECT amname FROM pg_am WHERE oid = c.relam),
        'row_security', c.relrowsecurity, 'force_row_security', c.relforcerowsecurity,
        'replica_identity', c.relreplident,
        'view', CASE WHEN c.relkind IN ('v', 'm') THEN pg_get_viewdef(c.oid) END,
        'partition_key', CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END,
        'partition_bound', pg_get_expr(c.relpartbound, c.oid),
        'parents', ARRAY(SELECT i.inhparent::regclass::text FROM pg_inherits i
            WHERE i.inhrelid = c.oid ORDER BY i.inhseqno),
        'columns', ARRAY(SELECT jsonb_build_object(
            'number', a.attnum, 'name', a.attname,
            'type', format_type(a.atttypid, a.atttypmod),
            'not_null', a.attnotnull, 'identity', a.attidentity, 'generated', a.attgenerated,
            'collation', a.attcollation::regcollation::text,
            'storage', a.attstorage, 'compression', a.attcompression,
            'default', pg_get_expr(d.adbin, c.oid), 'acl', a.attacl)
            FROM pg_attribute a LEFT JOIN pg_attrdef d
                ON d.adrelid = a.attrelid AND d.adnum = a.attnum
            WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum),
        'constraints', ARRAY(SELECT jsonb_build_object(
            'name', k.conname, 'definition', pg_get_constraintdef(k.oid),
            'validated', k.convalidated)
            FROM pg_constraint k WHERE k.conrelid = c.oid ORDER BY k.conname),
        'indexes', ARRAY(SELECT jsonb_build_object(
            'name', i.indexrelid::regclass::text, 'definition', pg_get_indexdef(i.indexrelid),
            'valid', i.indisvalid, 'clustered', i.indisclustered,
            'replica_identity', i.indisreplident)
            FROM pg_index i WHERE i.indrelid = c.oid ORDER BY i.indexrelid::regclass::text),
        'triggers', ARRAY(SELECT jsonb_build_object(
            'name', t.tgname, 'definition', pg_get_triggerdef(t.oid), 'enabled', t.tgenabled)
            FROM pg_trigger t WHERE t.tgrelid = c.oid AND NOT t.tgisinternal ORDER BY t.tgname),
        'rules', ARRAY(SELECT jsonb_build_object('definition', pg_get_ruledef(r.oid),
            'enabled', r.ev_enabled) FROM pg_rewrite r
            WHERE r.ev_class = c.oid AND r.rulename <> '_RETURN' ORDER BY r.rulename),
        'policies', ARRAY(SELECT jsonb_build_object(
            'name', p.polname, 'command', p.polcmd, 'permissive', p.polpermissive,
            'roles', ARRAY(SELECT CASE WHEN role_oid = 0 THEN 'PUBLIC'
                                       ELSE pg_get_userbyid(role_oid) END
                FROM unnest(p.polroles) AS member(role_oid) ORDER BY 1),
            'using', pg_get_expr(p.polqual, c.oid), 'check', pg_get_expr(p.polwithcheck, c.oid))
            FROM pg_policy p WHERE p.polrelid = c.oid ORDER BY p.polname),
        'sequence', (SELECT (to_jsonb(s) - ARRAY['seqrelid', 'seqtypid'])
            || jsonb_build_object('type', format_type(seqtypid, NULL))
            FROM pg_sequence s WHERE s.seqrelid = c.oid),
        'sequence_owned_by', (SELECT format('%s.%I', d.refobjid::regclass, a.attname)
            FROM pg_depend d JOIN pg_attribute a
                ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid
            WHERE c.relkind = 'S' AND d.classid = 'pg_class'::regclass AND d.objid = c.oid
              AND d.refclassid = 'pg_class'::regclass AND d.deptype IN ('a', 'i'))
    ) FROM relations c ORDER BY c.schema_name, c.relname),
    'routines', ARRAY(SELECT jsonb_build_object(
        'schema', p.schema_name, 'signature', p.signature,
        'owner', pg_get_userbyid(p.proowner), 'acl', p.proacl,
        'definition', CASE WHEN p.prokind <> 'a' THEN pg_get_functiondef(p.oid) END)
        FROM routines p ORDER BY p.schema_name, p.signature),
    'default_acls', ARRAY(SELECT jsonb_build_object(
        'schema', d.nspname, 'owner', pg_get_userbyid(d.defaclrole),
        'object_type', d.defaclobjtype, 'acl', d.defaclacl)
        FROM app_defaults d ORDER BY d.nspname, pg_get_userbyid(d.defaclrole), d.defaclobjtype),
    'roles', ARRAY(SELECT jsonb_build_object(
        'name', rolname, 'login', rolcanlogin, 'inherit', rolinherit,
        'superuser', rolsuper, 'create_role', rolcreaterole, 'create_database', rolcreatedb,
        'replication', rolreplication, 'bypass_rls', rolbypassrls,
        'connection_limit', rolconnlimit, 'valid_until', rolvaliduntil)
        FROM pg_roles WHERE oid IN (SELECT oid FROM reported_roles) ORDER BY rolname),
    'memberships', ARRAY(SELECT jsonb_build_object(
        'group', pg_get_userbyid(roleid), 'member', pg_get_userbyid(member),
        'grantor', pg_get_userbyid(grantor), 'admin', admin_option,
        'inherit', to_jsonb(m)->'inherit_option', 'set', to_jsonb(m)->'set_option')
        FROM pg_auth_members m WHERE member IN (SELECT oid FROM reported_roles)
        ORDER BY pg_get_userbyid(roleid),
            pg_get_userbyid(member), pg_get_userbyid(grantor)),
    'cutover_ledger', ARRAY(SELECT jsonb_build_object(
        'version', version, 'description', description, 'success', success,
        'checksum', encode(checksum, 'hex'), 'installed_on', installed_on,
        'execution_time', execution_time)
        FROM public._sqlx_migrations ORDER BY version),
    'store_ledger', ARRAY(SELECT jsonb_build_object(
        'version', version, 'description', description, 'success', success,
        'checksum', encode(checksum, 'hex'), 'installed_on', installed_on,
        'execution_time', execution_time)
        FROM public._two_bot_migrations ORDER BY version)
);
COMMIT;
