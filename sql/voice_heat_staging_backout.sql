-- Separately authorized staging recovery only; see docs/database-roles.md.
DO $$
BEGIN
    UPDATE public.web_contract_meta SET contract_version = '1.0'
     WHERE singleton AND contract_version = '1.1';
    IF NOT FOUND THEN
        RAISE EXCEPTION 'voice heat backout requires contract 1.1';
    END IF;
    REVOKE SELECT ON web_v1.voice_heat FROM two_web_reader;
END;
$$;
