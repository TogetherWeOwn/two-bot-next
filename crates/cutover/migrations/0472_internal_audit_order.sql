-- Drain old inserts before seeding; keep this lock through trigger activation.
-- SQLx applies this migration in one transaction. Existing tuples never change.
LOCK TABLE public.internal_action_log IN SHARE ROW EXCLUSIVE MODE;
CREATE TABLE public.internal_audit_order (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    last_created_at TIMESTAMPTZ NOT NULL CHECK (
        isfinite(last_created_at) AND last_created_at >= TIMESTAMPTZ '1970-01-01 00:00:00+00'
        AND extract(epoch FROM last_created_at) * 1000000 <= 9223372036854775807),
    last_audit_id BIGINT NOT NULL CHECK (last_audit_id >= 0)
);
INSERT INTO public.internal_audit_order (last_created_at, last_audit_id)
SELECT greatest(coalesce(max(created_at), TIMESTAMPTZ '1970-01-01 00:00:00+00'),
                         TIMESTAMPTZ '1970-01-01 00:00:00+00'),
       greatest(coalesce(max(audit_id), 0), 0)
FROM public.internal_action_log;

CREATE OR REPLACE FUNCTION public.internal_audit_allocate_order() RETURNS trigger
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
    -- Sample only after obtaining the global fence. The UPDATE lock is held
    -- until outer commit/rollback, including direct SQL and old-image inserts.
    allocated_time := greatest(last_time, clock_timestamp());
    IF NOT isfinite(allocated_time)
       OR extract(epoch FROM allocated_time) * 1000000 > 9223372036854775807 THEN
        RAISE EXCEPTION 'internal audit time outside cursor range';
    END IF;
    NEW.created_at := allocated_time;
    NEW.audit_id := last_id + 1;
    UPDATE public.internal_audit_order
    SET last_created_at = allocated_time, last_audit_id = NEW.audit_id WHERE singleton;
    RETURN NEW;
END;
$body$;
REVOKE ALL ON FUNCTION public.internal_audit_allocate_order() FROM PUBLIC;

CREATE OR REPLACE FUNCTION public.internal_audit_order_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, public AS $body$
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
$body$;
REVOKE ALL ON FUNCTION public.internal_audit_order_guard() FROM PUBLIC;
CREATE TRIGGER internal_audit_order_guard BEFORE UPDATE OR DELETE ON public.internal_audit_order
    FOR EACH ROW EXECUTE FUNCTION public.internal_audit_order_guard();
CREATE TRIGGER internal_audit_order_truncate_guard BEFORE TRUNCATE ON public.internal_audit_order
    FOR EACH STATEMENT EXECUTE FUNCTION public.internal_audit_order_guard();
CREATE TRIGGER internal_audit_allocate_order BEFORE INSERT ON public.internal_action_log
    FOR EACH ROW EXECUTE FUNCTION public.internal_audit_allocate_order();
