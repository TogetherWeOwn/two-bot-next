-- The history proof must still fence writers when the scoped changes commit.
CREATE FUNCTION public.fixture_check_history_lock() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF NOT EXISTS (
        SELECT FROM pg_locks
        WHERE locktype = 'relation'
          AND relation = 'public.backfill_history_messages'::regclass
          AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
          AND pid = pg_backend_pid()
          AND mode = 'ShareLock' AND granted
    ) THEN
        RAISE EXCEPTION 'history writer lock missing at privacy commit';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER fixture_check_history_lock
AFTER DELETE ON public.members DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION public.fixture_check_history_lock();

-- Scheduled member deletion and guild purge need different rows.
INSERT INTO public.members (guild_id, member_id)
VALUES ('100000000000000001', '200000000000000003');
