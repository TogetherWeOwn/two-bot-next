-- Print-only operator plan. Apply after bot migrations and sql/web_v1.sql.
-- NOLOGIN groups only: passwords and login membership are provisioned separately.
-- Rendered by `two-bot db roles plan [--phase full|bootstrap]`. The bootstrap
-- phase skips relations and sequences absent from the database; functions stay
-- strict in both phases. The full (default) phase raises on any absent object.
BEGIN;
SET LOCAL search_path = pg_catalog, pg_temp;

DO $roles$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_bot_migrator') THEN
        CREATE ROLE two_bot_migrator NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_bot_runtime') THEN
        CREATE ROLE two_bot_runtime NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_web_reader') THEN
        CREATE ROLE two_web_reader NOLOGIN;
    END IF;
    IF EXISTS (
        SELECT FROM pg_roles WHERE rolname IN ('two_bot_migrator', 'two_bot_runtime', 'two_web_reader')
        AND (rolcanlogin OR rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls)
    ) OR EXISTS (
        SELECT FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.member
        WHERE r.rolname IN ('two_bot_migrator', 'two_bot_runtime', 'two_web_reader')
    ) THEN
        RAISE EXCEPTION 'unsafe existing group attributes or outgoing memberships; refusing role plan';
    END IF;
END
$roles$;

-- Ephemeral membership for the executing identity. A non-superuser CREATEROLE
-- identity holds only ADMIN OPTION on the group it just created, so
-- `ALTER SCHEMA public OWNER TO two_bot_migrator` fails with
-- "must be able to SET ROLE", and the identity loses USAGE on `public` once
-- ownership flips. Grant SET/USAGE to `current_user` for this transaction
-- only; the matching REVOKE before COMMIT restores the pre-plan state.
DO $membership$
DECLARE
    modern boolean := current_setting('server_version_num')::integer >= 160000;
    usable boolean;
BEGIN
    IF modern THEN
        usable := pg_has_role(current_user, 'two_bot_migrator', 'SET')
              AND pg_has_role(current_user, 'two_bot_migrator', 'USAGE');
    ELSE
        usable := pg_has_role(current_user, 'two_bot_migrator', 'MEMBER');
    END IF;
    IF NOT usable THEN
        IF modern THEN
            EXECUTE format('GRANT %I TO %I WITH INHERIT TRUE, SET TRUE', 'two_bot_migrator', current_user);
        ELSE
            EXECUTE format('GRANT %I TO %I', 'two_bot_migrator', current_user);
        END IF;
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
    EXECUTE format('REVOKE ALL ON DATABASE %I FROM PUBLIC, two_bot_migrator, two_bot_runtime, two_web_reader', current_database());
    EXECUTE format('GRANT CONNECT, CREATE ON DATABASE %I TO two_bot_migrator', current_database());
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO two_bot_runtime, two_web_reader', current_database());
END
$database$;

-- Database-wide PUBLIC hardening affects other consumers of a shared database.
-- The operator must give unrelated services their own explicit grants first.
REVOKE ALL ON SCHEMA public FROM PUBLIC, two_bot_runtime, two_web_reader;
ALTER SCHEMA public OWNER TO two_bot_migrator;
GRANT USAGE ON SCHEMA public TO two_bot_runtime;
REVOKE ALL ON SCHEMA web_v1 FROM PUBLIC, two_bot_runtime, two_web_reader;
ALTER SCHEMA web_v1 OWNER TO two_bot_migrator;
GRANT USAGE ON SCHEMA web_v1 TO two_web_reader;

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
            EXECUTE format('REVOKE ALL ON FUNCTION %s FROM PUBLIC, two_bot_runtime, two_web_reader', target);
            EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO two_bot_migrator', target);
            IF obj.schema_name = 'web_v1' THEN
                EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO two_web_reader', target);
            END IF;
        ELSE
            target := format('%I.%I', obj.schema_name, obj.name);
            IF obj.kind = 'sequence' THEN
                EXECUTE format('ALTER SEQUENCE %s OWNER TO two_bot_migrator', target);
                EXECUTE format('REVOKE ALL ON SEQUENCE %s FROM PUBLIC, two_bot_runtime, two_web_reader', target);
                EXECUTE format('GRANT ALL ON SEQUENCE %s TO two_bot_migrator', target);
                EXECUTE format('GRANT USAGE, SELECT ON SEQUENCE %s TO two_bot_runtime', target);
            ELSE
                EXECUTE format('ALTER %s %s OWNER TO two_bot_migrator',
                    CASE WHEN obj.kind = 'view' THEN 'VIEW' ELSE 'TABLE' END, target);
                EXECUTE format('REVOKE ALL ON TABLE %s FROM PUBLIC, two_bot_runtime, two_web_reader', target);
                -- Ownership retains grant authority, not revoked ordinary rights.
                EXECUTE format('GRANT ALL ON TABLE %s TO two_bot_migrator', target);
                IF obj.kind = 'table' THEN
                    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE %s TO two_bot_runtime', target);
                ELSIF obj.kind = 'admission' THEN
                    -- Runtime may reserve/complete holds, never erase the lane.
                    EXECUTE format('GRANT SELECT, INSERT, UPDATE ON TABLE %s TO two_bot_runtime', target);
                ELSIF obj.kind = 'view' THEN
                    EXECUTE format('GRANT SELECT ON TABLE %s TO two_web_reader', target);
                END IF;
                -- SERIAL/IDENTITY sequences follow only allowlisted bot tables.
                FOR seq IN (
                    SELECT c.oid::regclass AS name FROM pg_class c
                    JOIN pg_depend d ON d.objid = c.oid AND d.classid = 'pg_class'::regclass
                    WHERE obj.kind = 'table' AND c.relkind = 'S' AND d.refobjid = to_regclass(target)
                      AND d.refclassid = 'pg_class'::regclass AND d.deptype IN ('a', 'i')
                ) LOOP
                    EXECUTE format('REVOKE ALL ON SEQUENCE %s FROM PUBLIC, two_bot_runtime, two_web_reader', seq.name);
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
ALTER DEFAULT PRIVILEGES FOR ROLE two_bot_migrator REVOKE ALL ON TABLES FROM PUBLIC, two_bot_runtime, two_web_reader;
ALTER DEFAULT PRIVILEGES FOR ROLE two_bot_migrator REVOKE ALL ON SEQUENCES FROM PUBLIC, two_bot_runtime, two_web_reader;

-- Drop the ephemeral self-grant before COMMIT so the executing identity keeps
-- only its pre-plan memberships (e.g. the creator ADMIN OPTION row).
REVOKE two_bot_migrator FROM CURRENT_USER;

COMMIT;
