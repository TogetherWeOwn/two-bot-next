-- Contract after 0469 and a stable production release of its authority change.
-- The command gate and configuration store use voice_access_controls.command_roles.
-- Backout is restore-from-backup only: the previous binary's /import still
-- writes the removed mirror. Do not label this binary-rollback-compatible.
DROP TABLE IF EXISTS voice_command_role_members;
DROP TABLE IF EXISTS voice_command_roles;
