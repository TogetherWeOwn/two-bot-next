-- The fixture is empty of member data. Sequences record attempted effects
-- even when the migrator transaction rolls back.
CREATE SCHEMA fixture_release_order;
CREATE SEQUENCE fixture_release_order.cutover_read;
CREATE SEQUENCE fixture_release_order.store_read;
CREATE SEQUENCE fixture_release_order.ddl_attempt;
CREATE SEQUENCE fixture_release_order.grant_attempt;
-- The setup session takes the real SQLx migration lock before loading this fixture.
CREATE TABLE fixture_release_order.writer_advisory_key AS
    SELECT classid, objid, objsubid FROM pg_locks
    WHERE pid = pg_backend_pid() AND granted AND locktype = 'advisory' AND mode = 'ExclusiveLock';
GRANT USAGE ON SCHEMA fixture_release_order TO two_bot_migrator;
GRANT SELECT ON fixture_release_order.writer_advisory_key TO two_bot_migrator;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA fixture_release_order TO two_bot_migrator;

CREATE FUNCTION fixture_release_order.read_ledger(marker regclass) RETURNS boolean
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF current_user <> 'two_bot_migrator' OR NOT EXISTS (
        SELECT FROM pg_locks l JOIN fixture_release_order.writer_advisory_key k
            USING (classid, objid, objsubid)
        WHERE l.pid = pg_backend_pid() AND l.granted
          AND l.locktype = 'advisory' AND l.mode = 'ExclusiveLock'
    ) OR 3 <> (
        SELECT count(*) FROM pg_locks WHERE pid = pg_backend_pid() AND granted
          AND locktype = 'relation' AND mode = 'ShareRowExclusiveLock'
          AND relation IN ('public._sqlx_migrations'::regclass,
                           'public._two_bot_migrations'::regclass,
                           'public.web_contract_meta'::regclass)
    ) THEN
        RAISE EXCEPTION 'release ledger read without migrator writer locks';
    END IF;
    PERFORM nextval(marker);
    RETURN true;
END;
$$;

ALTER TABLE public._sqlx_migrations ENABLE ROW LEVEL SECURITY;
ALTER TABLE public._sqlx_migrations FORCE ROW LEVEL SECURITY;
CREATE POLICY fixture_release_read ON public._sqlx_migrations FOR SELECT
    USING ((SELECT fixture_release_order.read_ledger('fixture_release_order.cutover_read')));
CREATE POLICY fixture_release_insert ON public._sqlx_migrations FOR INSERT WITH CHECK (true);
CREATE POLICY fixture_release_update ON public._sqlx_migrations FOR UPDATE USING (true);

ALTER TABLE public._two_bot_migrations ENABLE ROW LEVEL SECURITY;
ALTER TABLE public._two_bot_migrations FORCE ROW LEVEL SECURITY;
CREATE POLICY fixture_release_read ON public._two_bot_migrations FOR SELECT
    USING ((SELECT fixture_release_order.read_ledger('fixture_release_order.store_read')));
CREATE POLICY fixture_release_insert ON public._two_bot_migrations FOR INSERT WITH CHECK (true);
CREATE POLICY fixture_release_update ON public._two_bot_migrations FOR UPDATE USING (true);

CREATE FUNCTION fixture_release_order.observe_mutation() RETURNS event_trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF current_user = 'two_bot_migrator' THEN
        PERFORM nextval(CASE WHEN TG_TAG = 'GRANT' THEN 'fixture_release_order.grant_attempt'::regclass
                             ELSE 'fixture_release_order.ddl_attempt'::regclass END);
        IF NOT ((SELECT is_called FROM fixture_release_order.cutover_read)
                AND (SELECT is_called FROM fixture_release_order.store_read)) THEN
            RAISE EXCEPTION 'release mutation before both locked ledger reads';
        END IF;
    END IF;
END;
$$;
CREATE EVENT TRIGGER fixture_release_mutation ON ddl_command_start
    EXECUTE FUNCTION fixture_release_order.observe_mutation();
