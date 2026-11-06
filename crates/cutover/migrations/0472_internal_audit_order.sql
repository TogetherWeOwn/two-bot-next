-- rollback-compatible: prior-image audit INSERTs use the retained invoker ordering trigger with runtime table rights; no prior direct helper caller loses EXECUTE, and image rollback retains schema, triggers and allocator state.
-- Whole-file argument and target-specific release proof: docs/migrations.md.
-- Drain old inserts before seeding; keep this lock through trigger activation.
-- SQLx applies this migration in one transaction. Existing tuples never change.
DO $audit$
DECLARE
    schema_name TEXT := pg_catalog.current_schema();
    caller_path TEXT := pg_catalog.current_setting('search_path');
    visible_log OID := pg_catalog.to_regclass('internal_action_log');
    visible_intent OID := pg_catalog.to_regclass('internal_idempotency');
BEGIN
    -- Capture caller bindings before pinning catalog-first name resolution.
    PERFORM pg_catalog.set_config('search_path', 'pg_catalog', true);
    IF schema_name IS NULL OR schema_name !~ '^[a-z_][a-z0-9_]{0,58}$'
       OR left(schema_name, 3) = 'pg_' OR schema_name = 'information_schema' THEN
        RAISE EXCEPTION 'unsafe internal audit schema';
    END IF;
    IF to_regclass(format('%I.internal_action_log', schema_name)) IS NULL
       OR to_regclass(format('%I.internal_idempotency', schema_name)) IS NULL
       OR visible_log IS DISTINCT FROM to_regclass(format('%I.internal_action_log', schema_name))::oid
       OR visible_intent IS DISTINCT FROM to_regclass(format('%I.internal_idempotency', schema_name))::oid THEN
        RAISE EXCEPTION 'internal audit schema binding mismatch';
    END IF;
    IF EXISTS (SELECT FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
               WHERE n.nspname = schema_name
                 AND p.proname IN ('internal_audit_allocate_order', 'internal_audit_order_guard')
                 AND (p.pronargs <> 0 OR p.prokind <> 'f'
                      OR p.proowner <> (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = current_user))) THEN
        RAISE EXCEPTION 'internal audit helper signature or owner mismatch';
    END IF;
    IF to_regclass(format('%I.internal_audit_order', schema_name)) IS NOT NULL THEN
        RAISE EXCEPTION 'internal audit order already exists';
    END IF;
    EXECUTE format($ddl$LOCK TABLE %I.internal_action_log IN SHARE ROW EXCLUSIVE MODE$ddl$, schema_name);
    EXECUTE format($ddl$CREATE TABLE %I.internal_audit_order (
        singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
        last_created_at TIMESTAMPTZ NOT NULL CHECK (
            isfinite(last_created_at) AND last_created_at >= TIMESTAMPTZ '1970-01-01 00:00:00+00'
            AND extract(epoch FROM last_created_at) * 1000000 <= 9223372036854775807),
        last_audit_id BIGINT NOT NULL CHECK (last_audit_id >= 0)
    )$ddl$, schema_name);
    EXECUTE format($ddl$INSERT INTO %I.internal_audit_order (last_created_at, last_audit_id)
    SELECT greatest(coalesce(max(created_at), TIMESTAMPTZ '1970-01-01 00:00:00+00'),
                             TIMESTAMPTZ '1970-01-01 00:00:00+00'),
           greatest(coalesce(max(audit_id), 0), 0)
    FROM %I.internal_action_log$ddl$, schema_name, schema_name);

    EXECUTE replace($definition$CREATE OR REPLACE FUNCTION @audit_schema@.internal_audit_allocate_order() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, @audit_schema@ AS $body$
DECLARE
    last_time TIMESTAMPTZ;
    last_id BIGINT;
    allocated_time TIMESTAMPTZ;
BEGIN
    SELECT last_created_at, last_audit_id INTO STRICT last_time, last_id
    FROM @audit_schema@.internal_audit_order WHERE singleton FOR UPDATE;
    IF last_id < 0 OR last_id = 9223372036854775807 OR NOT isfinite(last_time)
       OR last_time < TIMESTAMPTZ '1970-01-01 00:00:00+00'
       OR extract(epoch FROM last_time) * 1000000 > 9223372036854775807 THEN
        RAISE EXCEPTION 'invalid or exhausted internal audit order';
    END IF;
    -- Sample only after obtaining the global fence. The UPDATE lock is held
    -- until outer commit/rollback, including direct SQL and old-image inserts.
    allocated_time := greatest(last_time, clock_timestamp());
    IF NOT isfinite(allocated_time)
       OR extract(epoch FROM allocated_time) * 1000000 > 9223372036854775807 THEN
        RAISE EXCEPTION 'internal audit time outside cursor range';
    END IF;
    NEW.created_at := allocated_time;
    NEW.audit_id := last_id + 1;
    UPDATE @audit_schema@.internal_audit_order
    SET last_created_at = allocated_time, last_audit_id = NEW.audit_id WHERE singleton;
    RETURN NEW;
END;
$body$;$definition$, '@audit_schema@', quote_ident(schema_name));
    EXECUTE format($ddl$REVOKE ALL ON FUNCTION %I.internal_audit_allocate_order() FROM PUBLIC$ddl$, schema_name);

    EXECUTE replace($definition$CREATE OR REPLACE FUNCTION @audit_schema@.internal_audit_order_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, @audit_schema@ AS $body$
BEGIN
    IF TG_OP <> 'UPDATE' THEN
        RAISE EXCEPTION 'internal audit order cannot be removed';
    END IF;
    IF NEW.singleton IS DISTINCT FROM OLD.singleton
       OR NEW.last_created_at < OLD.last_created_at
       OR NEW.last_audit_id < OLD.last_audit_id THEN
        RAISE EXCEPTION 'internal audit order cannot move backwards';
    END IF;
    RETURN NEW;
END;
$body$;$definition$, '@audit_schema@', quote_ident(schema_name));
    EXECUTE format($ddl$REVOKE ALL ON FUNCTION %I.internal_audit_order_guard() FROM PUBLIC$ddl$, schema_name);
    EXECUTE format($ddl$CREATE TRIGGER internal_audit_order_guard BEFORE UPDATE OR DELETE ON %I.internal_audit_order
        FOR EACH ROW EXECUTE FUNCTION %I.internal_audit_order_guard();
    CREATE TRIGGER internal_audit_order_truncate_guard BEFORE TRUNCATE ON %I.internal_audit_order
        FOR EACH STATEMENT EXECUTE FUNCTION %I.internal_audit_order_guard();
    CREATE TRIGGER internal_audit_allocate_order BEFORE INSERT ON %I.internal_action_log
        FOR EACH ROW EXECUTE FUNCTION %I.internal_audit_allocate_order();$ddl$,
        schema_name, schema_name, schema_name, schema_name, schema_name, schema_name);
    PERFORM set_config('search_path', caller_path, true);
END;
$audit$;
