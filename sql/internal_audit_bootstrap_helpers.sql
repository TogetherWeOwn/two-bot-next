-- Explicit pre-bootstrap preparation only. Print, review, then apply as the
-- existing provisioning identity. Never run automatically from verify/startup.
-- Definitions are duplicated deliberately: migrations remain self-contained.
BEGIN;
SET LOCAL search_path = pg_catalog, public;
DO $prepare$
DECLARE
    helper RECORD;
    existing pg_proc%ROWTYPE;
    helper_oid OID;
    ledger TEXT;
    introduced BOOLEAN;
BEGIN
    FOR helper IN SELECT * FROM (VALUES
        (473, 'internal_audit_identity_immutable', $definition$CREATE OR REPLACE FUNCTION public.internal_audit_identity_immutable() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, public AS $body$
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
$body$;$definition$),
        (474, 'internal_audit_allocate_order', $definition$CREATE OR REPLACE FUNCTION public.internal_audit_allocate_order() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, public AS $body$
DECLARE
    last_time TIMESTAMPTZ;
    last_id BIGINT;
    allocated_time TIMESTAMPTZ;
BEGIN
    SELECT last_created_at, last_audit_id INTO STRICT last_time, last_id
    FROM public.internal_audit_order WHERE singleton FOR UPDATE;
    IF last_id < 0 OR last_id = 9223372036854775807 OR NOT isfinite(last_time)
       OR last_time < TIMESTAMPTZ '1970-01-01 00:00:00+00'
       OR extract(epoch FROM last_time) * 1000000 > 9223372036854775807 THEN
        RAISE EXCEPTION 'invalid or exhausted internal audit order';
    END IF;
    -- The guarded UPDATE owns the sole clock sample after obtaining the fence.
    -- Its lock lasts until outer commit/rollback, including old-image inserts.
    NEW.audit_id := last_id + 1;
    UPDATE public.internal_audit_order
    SET last_audit_id = NEW.audit_id WHERE singleton
    RETURNING last_created_at INTO STRICT allocated_time;
    IF NOT isfinite(allocated_time)
       OR extract(epoch FROM allocated_time) * 1000000 > 9223372036854775807 THEN
        RAISE EXCEPTION 'internal audit time outside cursor range';
    END IF;
    NEW.created_at := allocated_time;
    RETURN NEW;
END;
$body$;$definition$),
        (474, 'internal_audit_order_guard', $definition$CREATE OR REPLACE FUNCTION public.internal_audit_order_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, public AS $body$
BEGIN
    IF TG_OP <> 'UPDATE' THEN
        RAISE EXCEPTION 'internal audit order cannot be removed';
    END IF;
    IF NEW.singleton IS DISTINCT FROM OLD.singleton
       OR NEW.last_created_at IS DISTINCT FROM OLD.last_created_at
       OR NEW.last_audit_id < OLD.last_audit_id
       OR NEW.last_audit_id::numeric > OLD.last_audit_id::numeric + 1 THEN
        RAISE EXCEPTION 'invalid internal audit order transition';
    END IF;
    IF NEW.last_audit_id > OLD.last_audit_id THEN
        NEW.last_created_at := greatest(OLD.last_created_at, clock_timestamp());
    END IF;
    RETURN NEW;
END;
$body$;$definition$)
    ) AS helpers(version, name, definition) LOOP
        -- Exact membership in either supported ledger; never max(version).
        FOREACH ledger IN ARRAY ARRAY['public._sqlx_migrations', 'public._two_bot_migrations'] LOOP
            IF to_regclass(ledger) IS NOT NULL THEN
                EXECUTE format('SELECT EXISTS (SELECT FROM %s WHERE version = $1)', ledger)
                    INTO introduced USING helper.version;
                IF introduced THEN
                    RAISE EXCEPTION 'audit helper preparation after migration % refused', helper.version;
                END IF;
            END IF;
        END LOOP;
        IF (SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
            WHERE n.nspname = 'public' AND p.proname = helper.name) > 1 THEN
            RAISE EXCEPTION 'audit helper overload refused: %', helper.name;
        END IF;
        helper_oid := to_regprocedure(format('public.%I()', helper.name));
        IF helper_oid IS NULL THEN
            IF EXISTS (SELECT FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                       WHERE n.nspname = 'public' AND p.proname = helper.name) THEN
                RAISE EXCEPTION 'audit helper signature mismatch: %', helper.name;
            END IF;
            EXECUTE helper.definition;
            EXECUTE format('REVOKE ALL ON FUNCTION public.%I() FROM PUBLIC', helper.name);
            helper_oid := to_regprocedure(format('public.%I()', helper.name));
        END IF;
        SELECT * INTO STRICT existing FROM pg_proc WHERE oid = helper_oid;
        IF existing.proowner <> (SELECT oid FROM pg_roles WHERE rolname = current_user)
           OR existing.prorettype <> 'pg_catalog.trigger'::regtype
           OR existing.prolang <> (SELECT oid FROM pg_language WHERE lanname = 'plpgsql')
           OR existing.prokind <> 'f' OR existing.prosecdef OR existing.proisstrict
           OR existing.proleakproof OR existing.proretset
           OR existing.provolatile <> 'v' OR existing.proparallel <> 'u'
           OR existing.proconfig IS DISTINCT FROM ARRAY['search_path=pg_catalog, public']
           OR existing.prosrc IS DISTINCT FROM split_part(helper.definition, '$body$', 2)
           OR EXISTS (SELECT FROM aclexplode(coalesce(existing.proacl,
                        acldefault('f', existing.proowner))) acl
                      WHERE acl.grantee <> existing.proowner)
           OR EXISTS (SELECT FROM pg_depend WHERE refclassid = 'pg_proc'::regclass
                      AND refobjid = helper_oid)
           OR EXISTS (SELECT FROM pg_trigger WHERE tgfoid = helper_oid) THEN
            RAISE EXCEPTION 'audit helper definition, owner, ACL or activation mismatch: %', helper.name;
        END IF;
    END LOOP;
END;
$prepare$;
COMMIT;
