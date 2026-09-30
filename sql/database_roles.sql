-- Print-only operator plan. Apply after bot migrations and sql/web_v1.sql.
-- NOLOGIN groups only: passwords and login membership are provisioned separately.
BEGIN;

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

-- Relations are granted explicitly, not through permissive future-table defaults.
-- New migrations/views require a reviewed addition to this plan and verifier.
ALTER DEFAULT PRIVILEGES FOR ROLE two_bot_migrator REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
ALTER DEFAULT PRIVILEGES FOR ROLE two_bot_migrator REVOKE ALL ON TABLES FROM PUBLIC, two_bot_runtime, two_web_reader;
ALTER DEFAULT PRIVILEGES FOR ROLE two_bot_migrator REVOKE ALL ON SEQUENCES FROM PUBLIC, two_bot_runtime, two_web_reader;

COMMIT;
