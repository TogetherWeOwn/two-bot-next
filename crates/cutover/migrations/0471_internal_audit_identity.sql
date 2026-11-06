-- rollback-compatible: prior-image writes omit nullable audit keys and preserve pinned identity; only the new private invoker helper loses PUBLIC EXECUTE, while normal intent transitions and audit erasure remain valid.
-- Whole-file argument and target-specific release proof: docs/migrations.md.
-- Freeze publication identity at the action boundary, not through mutable maps.
-- NULL supports old images and unprovable history; never backfill guessed names.
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
    IF EXISTS (SELECT FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
               WHERE n.nspname = schema_name AND p.proname = 'internal_audit_identity_immutable'
                 AND (p.pronargs <> 0 OR p.prokind <> 'f'
                      OR p.proowner <> (SELECT oid FROM pg_roles WHERE rolname = current_user))) THEN
        RAISE EXCEPTION 'internal audit helper signature or owner mismatch';
    END IF;
    PERFORM set_config('search_path', format('pg_catalog, %I', schema_name), true);

    ALTER TABLE internal_idempotency
        ADD COLUMN audit_event_key TEXT,
        ADD COLUMN audit_rank_key TEXT;
    ALTER TABLE internal_action_log
        ADD COLUMN audit_event_key TEXT,
        ADD COLUMN audit_rank_key TEXT;

    ALTER TABLE internal_idempotency ADD CONSTRAINT internal_intent_event_key
        CHECK (audit_event_key IS NULL OR
            (action IN ('event.upsert', 'event.cancel') AND octet_length(audit_event_key) BETWEEN 1 AND 200
             AND audit_event_key COLLATE "C" ~ '^[A-Za-z0-9._:-]+$')) NOT VALID;
    ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_intent_event_key;
    ALTER TABLE internal_idempotency ADD CONSTRAINT internal_intent_rank_key
        CHECK (audit_rank_key IS NULL OR
            (action = 'role.assign' AND resolved_role_id IS NOT NULL
             AND audit_rank_key IN ('prospect', 'member', 'soldier', 'veteran', 'legend'))) NOT VALID;
    ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_intent_rank_key;
    ALTER TABLE internal_action_log ADD CONSTRAINT internal_log_event_key
        CHECK (audit_event_key IS NULL OR
            (action IN ('event.upsert', 'event.cancel') AND octet_length(audit_event_key) BETWEEN 1 AND 200
             AND audit_event_key COLLATE "C" ~ '^[A-Za-z0-9._:-]+$')) NOT VALID;
    ALTER TABLE internal_action_log VALIDATE CONSTRAINT internal_log_event_key;
    ALTER TABLE internal_action_log ADD CONSTRAINT internal_log_rank_key
        CHECK (audit_rank_key IS NULL OR
            (action = 'role.assign' AND resolved_role_id IS NOT NULL
             AND audit_rank_key IN ('prospect', 'member', 'soldier', 'veteran', 'legend'))) NOT VALID;
    ALTER TABLE internal_action_log VALIDATE CONSTRAINT internal_log_rank_key;

    -- Replays and reconciliation may not enrich a previously consumed audit row.
    -- Authorized erasure still DELETEs rows; this is not a blanket append-only gate.
    EXECUTE replace($definition$CREATE OR REPLACE FUNCTION @audit_schema@.internal_audit_identity_immutable() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, @audit_schema@ AS $body$
BEGIN
    IF TG_TABLE_NAME = 'internal_action_log' THEN
        IF NEW IS DISTINCT FROM OLD THEN
            RAISE EXCEPTION 'internal audit rows are immutable';
        END IF;
    ELSE
        IF NEW.audit_event_key IS DISTINCT FROM OLD.audit_event_key
           OR ((OLD.state = 'completed' OR OLD.audit_rank_key IS NOT NULL)
               AND NEW.audit_rank_key IS DISTINCT FROM OLD.audit_rank_key) THEN
            RAISE EXCEPTION 'internal audit identity is immutable';
        END IF;
    END IF;
    RETURN NEW;
END;
$body$;$definition$, '@audit_schema@', quote_ident(schema_name));
    EXECUTE format($ddl$REVOKE ALL ON FUNCTION %I.internal_audit_identity_immutable() FROM PUBLIC$ddl$, schema_name);
    EXECUTE format($ddl$CREATE TRIGGER internal_log_identity_immutable BEFORE UPDATE ON %I.internal_action_log
        FOR EACH ROW EXECUTE FUNCTION %I.internal_audit_identity_immutable()$ddl$, schema_name, schema_name);
    EXECUTE format($ddl$CREATE TRIGGER internal_intent_identity_immutable BEFORE UPDATE ON %I.internal_idempotency
        FOR EACH ROW EXECUTE FUNCTION %I.internal_audit_identity_immutable()$ddl$, schema_name, schema_name);
    PERFORM set_config('search_path', caller_path, true);
END;
$audit$;
