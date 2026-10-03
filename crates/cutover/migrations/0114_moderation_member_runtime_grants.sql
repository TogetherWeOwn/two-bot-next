-- Add member-ledger runtime access without changing applied migrations.
-- Bootstrap can precede role provisioning: the reviewed role plan grants these
-- same objects once the groups exist. On upgrades, only the existing runtime
-- group receives DML and sequence allocation/read privileges. No ownership,
-- schema, default-ACL, reader, grant-option or sequence UPDATE rights change.
-- Resolve the migration schema explicitly, including isolated test schemas.
DO $member_grants$
DECLARE
    schema_name text := current_schema();
    table_name text;
BEGIN
    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_bot_runtime') THEN
        FOREACH table_name IN ARRAY ARRAY[
            'moderation_warnings',
            'moderation_scheduled_unbans',
            'moderation_member_bans',
            'moderation_audit',
            'moderation_idempotency'
        ] LOOP
            EXECUTE format(
                'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE %I.%I TO two_bot_runtime',
                schema_name, table_name);
        END LOOP;
        EXECUTE format(
            'GRANT USAGE, SELECT ON SEQUENCE %I.moderation_member_bans_generation_seq TO two_bot_runtime',
            schema_name);
    END IF;
END
$member_grants$;
