-- AFTER INSERT proves the failure occurs after the real audit insert AND
-- after the member writes. A prematurely failing import cannot satisfy this
-- sentinel. All checks see the import transaction's own uncommitted changes.
CREATE FUNCTION mee6_fail_final_audit() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.guild_id <> '111111111111111111'
        OR NEW.inserted <> 1 OR NEW.updated <> 1 OR NEW.unchanged <> 1
        OR NOT EXISTS (
            SELECT 1 FROM member_levels
            WHERE guild_id = NEW.guild_id AND member_id = '100000000000000001'
                AND xp = 80 AND message_xp = 30 AND voice_xp = 10 AND imported_xp = 40
                AND updated_at = NEW.imported_at
        )
        OR NOT EXISTS (
            SELECT 1 FROM member_levels
            WHERE guild_id = NEW.guild_id AND member_id = '100000000000000005'
                AND xp = 50 AND message_xp = 0 AND voice_xp = 0 AND imported_xp = 50
                AND updated_at = NEW.imported_at
        )
        OR NOT EXISTS (SELECT 1 FROM level_import_runs WHERE id = NEW.id)
    THEN
        RAISE EXCEPTION 'fixture: final audit reached without expected member writes';
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'P0001',
        MESSAGE = 'fixture: final import audit insert failed after member writes';
END;
$$;
CREATE TRIGGER mee6_fail_final_audit AFTER INSERT ON level_import_runs
    FOR EACH ROW EXECUTE FUNCTION mee6_fail_final_audit();
