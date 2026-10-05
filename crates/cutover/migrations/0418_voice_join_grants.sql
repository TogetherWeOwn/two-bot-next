-- Voice V3 approved Connect grants (`/public` revocation witness).
--
-- An approved Join request grants a member Connect on the room only. Discord
-- keeps that overwrite across a worker restart, but the worker forgot which
-- members it had approved, so a later `/public` could not take the allow back.
-- This table is the durable witness: one row per (guild, room, approved
-- member), written as the approval's intent before the Discord PUT and retired
-- only after the revocation PUT lands (or the grant is definitively refused).
--
-- Lifecycle (see `GuildRoomWorker::dispatch_approve` / `dispatch_revoke`):
-- intent is inserted before granting Connect; a refused grant deletes its row
-- so the member can ask again; an unknown Discord outcome keeps its row so a
-- retry or a restart can still revoke; a refused or unknown revoke keeps its
-- row so revocation can continue after a restart. Rows never authorize erasing
-- unrelated member overwrites: revocation clears only the Connect bit and
-- preserves every other allow/deny bit, including vote-kick denies.
--
-- Rows go with the room row (ON DELETE CASCADE), including a room row removed
-- by member erasure. `created_at` stamps the approval for rollback measurement.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS voice_join_grants (
  guild_id          TEXT        NOT NULL,
  room_channel_id   TEXT        NOT NULL,
  member_id         TEXT        NOT NULL
    CHECK (member_id ~ '^[0-9]{1,20}$' AND member_id <> '0'),
  created_at        timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (guild_id, room_channel_id, member_id),
  FOREIGN KEY (guild_id, room_channel_id)
    REFERENCES voice_rooms (guild_id, channel_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_voice_join_grants_member
  ON voice_join_grants (guild_id, member_id);

-- Bootstrap may precede role provisioning. Upgrades grant only the existing
-- runtime group the same journal DML listed in the reviewed role matrix.
DO $join_grants$
BEGIN
    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'two_bot_runtime') THEN
        EXECUTE format(
            'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE %I.voice_join_grants TO two_bot_runtime',
            current_schema());
    END IF;
END
$join_grants$;
