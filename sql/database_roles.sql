-- Print-only operator plan. Apply after bot migrations and sql/web_v1.sql.
-- NOLOGIN groups only: passwords and login membership are provisioned separately.
-- Rendered by `two-bot db roles plan [--phase full|bootstrap]`. The bootstrap
-- phase skips relations and sequences absent from the database; functions stay
-- strict in both phases. The full (default) phase raises on any absent object.
BEGIN;
SET LOCAL search_path = pg_catalog, pg_temp;

DO $roles$
DECLARE
    migrator_role_exists boolean;
    preexisting_migrator_membership boolean := false;
BEGIN
    SELECT EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_bot_migrator')
    INTO migrator_role_exists;
    IF migrator_role_exists THEN
        SELECT EXISTS (
            SELECT FROM pg_auth_members m
            JOIN pg_roles granted ON granted.oid = m.roleid
            JOIN pg_roles member ON member.oid = m.member
            WHERE granted.rolname = 'two_bot_migrator' AND member.rolname = current_user
        ) INTO preexisting_migrator_membership;
    END IF;
    PERFORM set_config(
        'two_bot.roles.preexisting_migrator_membership',
        preexisting_migrator_membership::text,
        true
    );

    IF NOT migrator_role_exists THEN
        CREATE ROLE two_bot_migrator NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_bot_runtime') THEN
        CREATE ROLE two_bot_runtime NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_web_reader') THEN
        CREATE ROLE two_web_reader NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_bot_migrator_ro') THEN
        CREATE ROLE two_bot_migrator_ro NOLOGIN;
    END IF;
    IF EXISTS (
        SELECT FROM pg_roles WHERE rolname IN ('two_bot_migrator', 'two_bot_runtime', 'two_web_reader', 'two_bot_migrator_ro')
        AND (rolcanlogin OR rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls)
    ) OR EXISTS (
        SELECT FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.member
        WHERE r.rolname IN ('two_bot_migrator', 'two_bot_runtime', 'two_web_reader', 'two_bot_migrator_ro')
    ) THEN
        RAISE EXCEPTION 'unsafe existing group attributes or outgoing memberships; refusing role plan';
    END IF;
END
$roles$;

-- Ephemeral membership for the executing identity. A non-superuser CREATEROLE
-- identity may already hold a direct membership (the ADMIN-only row PostgreSQL
-- 16+ gives a role's creator, or an operator grant), so the first block records
-- whether one existed before role creation. A usable identity (a superuser
-- always is) gets no grant and no revoke. Otherwise the plan grants its own row
-- as CURRENT_USER and revokes only that row; it refuses when the unusable
-- pre-existing row was granted by the identity itself, because a second grant
-- would rewrite that row's options and the revoke would delete it.
DO $membership$
DECLARE
    modern boolean := current_setting('server_version_num')::integer >= 160000;
    usable boolean;
    preexisting_membership boolean := current_setting('two_bot.roles.preexisting_migrator_membership')::boolean;
    self_granted_membership boolean;
BEGIN
    PERFORM set_config('two_bot.roles.ephemeral_migrator_membership', 'false', true);
    IF modern THEN
        usable := pg_has_role(current_user, 'two_bot_migrator', 'SET')
              AND pg_has_role(current_user, 'two_bot_migrator', 'USAGE');
    ELSE
        usable := pg_has_role(current_user, 'two_bot_migrator', 'MEMBER');
    END IF;
    IF NOT usable THEN
        IF preexisting_membership THEN
            SELECT EXISTS (
                SELECT FROM pg_auth_members m
                JOIN pg_roles granted ON granted.oid = m.roleid
                JOIN pg_roles member ON member.oid = m.member
                JOIN pg_roles grantor ON grantor.oid = m.grantor
                WHERE granted.rolname = 'two_bot_migrator'
                  AND member.rolname = current_user
                  AND grantor.rolname = current_user
            ) INTO self_granted_membership;
            IF self_granted_membership THEN
                RAISE EXCEPTION 'pre-existing two_bot_migrator membership is not usable; refusing to change its options';
            END IF;
        END IF;
        IF modern THEN
            EXECUTE format('GRANT %I TO %I WITH INHERIT TRUE, SET TRUE GRANTED BY CURRENT_USER', 'two_bot_migrator', current_user);
        ELSE
            EXECUTE format('GRANT %I TO %I GRANTED BY CURRENT_USER', 'two_bot_migrator', current_user);
        END IF;
        PERFORM set_config('two_bot.roles.ephemeral_migrator_membership', 'true', true);
    END IF;
    IF modern THEN
        usable := pg_has_role(current_user, 'two_bot_migrator', 'SET')
              AND pg_has_role(current_user, 'two_bot_migrator', 'USAGE');
    ELSE
        usable := pg_has_role(current_user, 'two_bot_migrator', 'MEMBER');
    END IF;
    IF NOT usable THEN
        RAISE EXCEPTION 'executing identity cannot SET ROLE two_bot_migrator; refusing role plan';
    END IF;
END
$membership$;

DO $database$
BEGIN
    EXECUTE format('REVOKE ALL ON DATABASE %I FROM PUBLIC, two_bot_migrator, two_bot_runtime, two_web_reader, two_bot_migrator_ro', current_database());
    EXECUTE format('GRANT CONNECT, CREATE ON DATABASE %I TO two_bot_migrator', current_database());
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO two_bot_runtime, two_web_reader, two_bot_migrator_ro', current_database());
END
$database$;

-- Database-wide PUBLIC hardening affects other consumers of a shared database.
-- The operator must give unrelated services their own explicit grants first.
REVOKE ALL ON SCHEMA public FROM PUBLIC, two_bot_runtime, two_web_reader, two_bot_migrator_ro;
ALTER SCHEMA public OWNER TO two_bot_migrator;
GRANT USAGE ON SCHEMA public TO two_bot_runtime, two_bot_migrator_ro;
REVOKE ALL ON SCHEMA web_v1 FROM PUBLIC, two_bot_runtime, two_web_reader, two_bot_migrator_ro;
ALTER SCHEMA web_v1 OWNER TO two_bot_migrator;
GRANT USAGE ON SCHEMA web_v1 TO two_web_reader, two_bot_migrator_ro;

-- Rendered by `two-bot db roles plan` with the shared reviewed object matrix.
DO $objects$
DECLARE
    obj record;
    seq record;
    target text;
BEGIN
    FOR obj IN (
-- @matrix
    ) LOOP
-- @absent_relation
        IF obj.kind = 'function' THEN
            target := to_regprocedure(obj.schema_name || '.' || obj.name)::text;
            IF target IS NULL THEN
                RAISE EXCEPTION 'missing function: %.%', obj.schema_name, obj.name;
            END IF;
            EXECUTE format('ALTER FUNCTION %s OWNER TO two_bot_migrator', target);
            EXECUTE format('REVOKE ALL ON FUNCTION %s FROM PUBLIC, two_bot_runtime, two_web_reader, two_bot_migrator_ro', target);
            EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO two_bot_migrator', target);
            IF obj.schema_name = 'web_v1' THEN
                EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO two_web_reader', target);
            END IF;
        ELSE
            target := format('%I.%I', obj.schema_name, obj.name);
            IF obj.kind = 'sequence' THEN
                EXECUTE format('ALTER SEQUENCE %s OWNER TO two_bot_migrator', target);
                EXECUTE format('REVOKE ALL ON SEQUENCE %s FROM PUBLIC, two_bot_runtime, two_web_reader, two_bot_migrator_ro', target);
                EXECUTE format('GRANT ALL ON SEQUENCE %s TO two_bot_migrator', target);
                EXECUTE format('GRANT USAGE, SELECT ON SEQUENCE %s TO two_bot_runtime', target);
            ELSE
                EXECUTE format('ALTER %s %s OWNER TO two_bot_migrator',
                    CASE WHEN obj.kind = 'view' THEN 'VIEW' ELSE 'TABLE' END, target);
                EXECUTE format('REVOKE ALL ON TABLE %s FROM PUBLIC, two_bot_runtime, two_web_reader, two_bot_migrator_ro', target);
                -- Ownership retains grant authority, not revoked ordinary rights.
                EXECUTE format('GRANT ALL ON TABLE %s TO two_bot_migrator', target);
                IF obj.kind = 'table' THEN
                    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE %s TO two_bot_runtime', target);
                    -- Read-only migration-plan identity: SELECT only, never DML/DDL.
                    EXECUTE format('GRANT SELECT ON TABLE %s TO two_bot_migrator_ro', target);
                ELSIF obj.kind = 'admission' THEN
                    -- Runtime may reserve/complete holds, never erase the lane.
                    EXECUTE format('GRANT SELECT, INSERT, UPDATE ON TABLE %s TO two_bot_runtime', target);
                    EXECUTE format('GRANT SELECT ON TABLE %s TO two_bot_migrator_ro', target);
                ELSIF obj.kind = 'ledger' THEN
                    -- Plan runs read pending versions from the SQLx ledger.
                    EXECUTE format('GRANT SELECT ON TABLE %s TO two_bot_migrator_ro', target);
                ELSIF obj.kind = 'view' THEN
                    EXECUTE format('GRANT SELECT ON TABLE %s TO two_web_reader', target);
                ELSIF obj.kind = 'migrator' THEN
                    -- Migrator-only: owner/migrator ALL, no runtime/reader grant.
                    -- Covers operator audit and redirect-store tables that the
                    -- gateway and website must never read or write directly.
                    NULL;
                END IF;
                -- SERIAL/IDENTITY sequences follow only allowlisted runtime tables.
                FOR seq IN (
                    SELECT c.oid::regclass AS name FROM pg_class c
                    JOIN pg_depend d ON d.objid = c.oid AND d.classid = 'pg_class'::regclass
                    WHERE obj.kind = 'table' AND c.relkind = 'S' AND d.refobjid = to_regclass(target)
                      AND d.refclassid = 'pg_class'::regclass AND d.deptype IN ('a', 'i')
                ) LOOP
                    EXECUTE format('REVOKE ALL ON SEQUENCE %s FROM PUBLIC, two_bot_runtime, two_web_reader, two_bot_migrator_ro', seq.name);
                    EXECUTE format('GRANT USAGE, SELECT ON SEQUENCE %s TO two_bot_runtime', seq.name);
                END LOOP;
            END IF;
        END IF;
    END LOOP;
END
$objects$;

-- Relations are granted explicitly, not through permissive future-table defaults.
-- New migrations/views require a reviewed addition to this plan and verifier.
ALTER DEFAULT PRIVILEGES FOR ROLE two_bot_migrator REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
ALTER DEFAULT PRIVILEGES FOR ROLE two_bot_migrator REVOKE ALL ON TABLES FROM PUBLIC, two_bot_runtime, two_web_reader, two_bot_migrator_ro;
ALTER DEFAULT PRIVILEGES FOR ROLE two_bot_migrator REVOKE ALL ON SEQUENCES FROM PUBLIC, two_bot_runtime, two_web_reader, two_bot_migrator_ro;

-- Drop only rows this plan put in place under the executing identity's own
-- grantor. That is the explicit temporary grant made above, and, when the
-- identity held no direct membership before the plan, the usable self-grant
-- that PostgreSQL 16+ adds on CREATE ROLE when `createrole_self_grant` is set.
-- Rows with another grantor, including the creator ADMIN OPTION membership,
-- and every row that existed before the plan, remain unchanged.
DO $membership_cleanup$
BEGIN
    IF current_setting('two_bot.roles.ephemeral_migrator_membership', true) = 'true'
       OR (
            current_setting('two_bot.roles.preexisting_migrator_membership', true) = 'false'
            AND EXISTS (
                SELECT FROM pg_auth_members m
                JOIN pg_roles granted ON granted.oid = m.roleid
                JOIN pg_roles member ON member.oid = m.member
                JOIN pg_roles grantor ON grantor.oid = m.grantor
                WHERE granted.rolname = 'two_bot_migrator'
                  AND member.rolname = current_user
                  AND grantor.rolname = current_user
            )
       ) THEN
        EXECUTE format('REVOKE %I FROM %I GRANTED BY CURRENT_USER', 'two_bot_migrator', current_user);
    END IF;
END
$membership_cleanup$;

COMMIT;
