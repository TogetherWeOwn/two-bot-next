-- Record prospective owner-grant recipients before Discord writes. Pending
-- cleanup survives failed ownership SQL, retry exhaustion and worker restart.
-- A revision fences completion against a newer room-scoped preparation.
CREATE TABLE voice_owner_grants (
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    pending BOOLEAN NOT NULL DEFAULT TRUE,
    touched_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, channel_id, member_id),
    FOREIGN KEY (guild_id, channel_id)
        REFERENCES voice_rooms (guild_id, channel_id) ON DELETE CASCADE
);

CREATE INDEX voice_owner_grants_pending
    ON voice_owner_grants (guild_id, channel_id) WHERE pending;

-- Bootstrap may precede role provisioning. Upgrades grant only the existing
-- runtime group the same journal DML listed in the reviewed role matrix.
DO $owner_grants$
BEGIN
    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_bot_runtime') THEN
        EXECUTE format(
            'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE %I.voice_owner_grants TO two_bot_runtime',
            current_schema());
    END IF;
END
$owner_grants$;
