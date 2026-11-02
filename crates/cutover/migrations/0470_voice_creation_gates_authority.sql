-- 0470_voice_creation_gates_authority: one store for the guild-wide voice
-- creation switch and command role gate.
--
-- `/import` (0229) wrote `settings.creation_enabled` and
-- `settings.command_role_id` to `voice_guild_settings`, but the runtime
-- gates read only `voice_access_controls.room_creation_enabled` and
-- `required_role_id` (0227), so an imported and confirmed switch or role
-- was never enforced. The spec gives both pairs the same meaning
-- (`docs/voice-rooms.md` V10: turn room creation on or off while commands
-- keep working; an optional role required to use room commands), so
-- `voice_access_controls` is now the single authority, the same move 0469
-- made for per-command restrictions: `PgVoiceConfigStore` reads and
-- writes it, `/access` already did. Expand/contract: the
-- `voice_guild_settings` columns stay as a mirror, because the previous
-- binary's `/export` reads them; a later contract migration drops them
-- once this release is stable in production.
--
-- Merge rule, tighten only: creation stays off if either store says off,
-- and a guild-wide role already enforced wins over the imported one
-- (swapping roles would loosen the gate for holders of the imported
-- role). An imported role lands only where no gate is enforced today.
-- So the backfill never loosens a gate that is enforced today, and an
-- imported switch or role that was silently ignored takes effect. The
-- mirror is then synced to the merged values so the previous binary's
-- `/export` after a backout shows what is enforced; a hand-edited
-- non-canonical role ID (the 0227 CHECK allows a leading zero, the 0229
-- CHECK does not) skips the sync rather than failing the migration.
-- A re-run only re-tightens from the mirror, never loosens; migrations
-- run once. The first statement takes the same per-guild advisory lock
-- `PgVoiceConfigStore::apply` and `PgRoomStore::save_access_controls`
-- use (`voice_config:{guild}`), so an `/access` save against the still
-- running previous binary compares after the merge instead of landing
-- its whole-row write over it. Backout: deploy the previous binary; the
-- merged values stay enforced and its `/export` reads the synced mirror.
-- No member PII.
-- Bot schema range: 0001-0999. Database tests use test containers only.

SELECT pg_advisory_xact_lock(hashtextextended('voice_config:' || guild_id, 0))
  FROM voice_guild_settings
 ORDER BY guild_id;

INSERT INTO voice_access_controls (guild_id, room_creation_enabled, required_role_id)
SELECT guild_id, creation_enabled, command_role_id
  FROM voice_guild_settings
ON CONFLICT (guild_id) DO UPDATE
  SET room_creation_enabled =
        voice_access_controls.room_creation_enabled AND EXCLUDED.room_creation_enabled,
      required_role_id =
        COALESCE(voice_access_controls.required_role_id, EXCLUDED.required_role_id);

UPDATE voice_guild_settings s
   SET creation_enabled = a.room_creation_enabled,
       command_role_id = a.required_role_id
  FROM voice_access_controls a
 WHERE a.guild_id = s.guild_id
   AND (a.required_role_id IS NULL OR a.required_role_id NOT LIKE '0%');
