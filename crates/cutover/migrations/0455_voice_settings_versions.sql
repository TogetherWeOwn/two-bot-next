-- Voice configuration is one optimistic-concurrency unit per guild.
-- Existing rows have version zero and unknown attribution; do not invent actors.
CREATE TABLE voice_settings_audit (
    version BIGSERIAL PRIMARY KEY,
    guild_id TEXT NOT NULL,
    actor TEXT NOT NULL CHECK (char_length(actor) BETWEEN 1 AND 128),
    at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX voice_settings_audit_guild_version ON voice_settings_audit (guild_id, version DESC);

CREATE FUNCTION refuse_voice_audit_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'voice settings audit is append only';
END;
$$;
CREATE TRIGGER voice_settings_audit_append_only
    BEFORE UPDATE OR DELETE OR TRUNCATE ON voice_settings_audit
    FOR EACH STATEMENT EXECUTE FUNCTION refuse_voice_audit_mutation();

ALTER TABLE voice_creators ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_access_controls ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_logging_settings ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_guild_settings ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_channel_templates ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_game_aliases ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_random_lists ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_random_list_choices ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_logging ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_logging_mention_members ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_logging_mention_roles ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_command_roles ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();
ALTER TABLE voice_command_role_members ADD COLUMN version BIGINT DEFAULT 0, ADD COLUMN updated_by TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ DEFAULT clock_timestamp();

-- Stores hold the voice_config:<guild> transaction lock and set these local
-- fields before writing. No session-scoped state survives a pooled connection.
CREATE FUNCTION stamp_voice_settings_change() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    change_version TEXT := current_setting('two.voice_version', true);
    change_actor TEXT := current_setting('two.voice_actor', true);
    change_guild TEXT := current_setting('two.voice_guild', true);
BEGIN
    IF change_version IS NOT NULL AND change_version <> '' THEN
        IF change_guild IS DISTINCT FROM NEW.guild_id OR change_actor IS NULL OR change_actor = '' THEN
            RAISE EXCEPTION 'voice change context mismatch';
        END IF;
        NEW.version := change_version::BIGINT;
        NEW.updated_by := change_actor;
        NEW.updated_at := clock_timestamp();
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER voice_creators_stamp BEFORE INSERT OR UPDATE ON voice_creators
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_access_controls_stamp BEFORE INSERT OR UPDATE ON voice_access_controls
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_logging_settings_stamp BEFORE INSERT OR UPDATE ON voice_logging_settings
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_guild_settings_stamp BEFORE INSERT OR UPDATE ON voice_guild_settings
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_channel_templates_stamp BEFORE INSERT OR UPDATE ON voice_channel_templates
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_game_aliases_stamp BEFORE INSERT OR UPDATE ON voice_game_aliases
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_random_lists_stamp BEFORE INSERT OR UPDATE ON voice_random_lists
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_random_list_choices_stamp BEFORE INSERT OR UPDATE ON voice_random_list_choices
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_logging_stamp BEFORE INSERT OR UPDATE ON voice_logging
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_logging_mention_members_stamp BEFORE INSERT OR UPDATE ON voice_logging_mention_members
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_logging_mention_roles_stamp BEFORE INSERT OR UPDATE ON voice_logging_mention_roles
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_command_roles_stamp BEFORE INSERT OR UPDATE ON voice_command_roles
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();
CREATE TRIGGER voice_command_role_members_stamp BEFORE INSERT OR UPDATE ON voice_command_role_members
    FOR EACH ROW EXECUTE FUNCTION stamp_voice_settings_change();

ALTER TABLE internal_idempotency ADD COLUMN config_version BIGINT;
ALTER TABLE internal_idempotency DROP CONSTRAINT internal_idempotency_action_check;
ALTER TABLE internal_idempotency ADD CONSTRAINT internal_idempotency_action_check CHECK (action IN (
    'role.assign', 'guild.add_member', 'announcement.post',
    'event.upsert', 'event.cancel', 'event.read',
    'automations.import', 'automations.export', 'settings.get', 'settings.set',
    'voice.config.get', 'voice.config.set',
    'moderation.ban', 'moderation.tempban', 'moderation.kick',
    'moderation.timeout', 'moderation.warn', 'moderation.purge',
    'moderation.slowmode', 'moderation.lockdown', 'moderation.unlock'
)) NOT VALID;
ALTER TABLE internal_idempotency VALIDATE CONSTRAINT internal_idempotency_action_check;
