-- 0469_voice_command_roles_authority: one store for per-command role gates.
--
-- `/import` (0229) wrote per-command restrictions to `voice_command_roles` /
-- `voice_command_role_members`, but the command gate reads only
-- `voice_access_controls.command_roles` (0227), so an imported and confirmed
-- restriction was never enforced. `voice_access_controls.command_roles` is now
-- the single authority: `PgVoiceConfigStore` reads and writes it, `/access`
-- already did. This moves every legacy row there and empties the legacy
-- tables, which nothing reads any more.
--
-- Merge rule: a command already restricted in `voice_access_controls` keeps
-- its enforced role list; a command only the legacy tables restrict is added.
-- So the backfill never loosens a gate that is enforced today, and a legacy
-- command with no member rows lands as an empty list (deny every non-admin),
-- the same meaning both stores give it. Role lists are canonical (numeric
-- order). The deletes make a re-run a no-op. Backout: deploy the previous
-- binary; the moved restrictions stay enforced, but its `/export` and
-- `/import` no longer see them until they are re-imported. No member PII.
-- Bot schema range: 0001-0999. Database tests use test containers only.

INSERT INTO voice_access_controls (guild_id, command_roles)
SELECT c.guild_id,
       jsonb_object_agg(
         c.command,
         COALESCE(
           (SELECT jsonb_agg(m.role_id ORDER BY length(m.role_id), m.role_id)
              FROM voice_command_role_members m
             WHERE m.guild_id = c.guild_id AND m.command = c.command),
           '[]'::jsonb))
  FROM voice_command_roles c
 GROUP BY c.guild_id
ON CONFLICT (guild_id) DO UPDATE
  SET command_roles = EXCLUDED.command_roles || voice_access_controls.command_roles;

DELETE FROM voice_command_role_members;
DELETE FROM voice_command_roles;
