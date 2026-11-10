-- 0468_custom_command_registry_resync: durable pending-publication marker.
--
-- A custom-command define/remove/toggle commits its saved definition before
-- the one full registry is republished, so a refused registry PUT (Discord
-- outage, 403) used to leave the command picker stale until a bot restart
-- rebuilt it. This table persists that pending resynchronization instead:
-- a failed publication records one row per guild, and the next successful
-- full-set publication (a later management publish, a gateway READY sync or
-- the boot resync) republishes the complete saved set and deletes the row.
-- The committed mutation itself is never replayed.
--
-- Runtime state only: a guild id and the first failure time, no member data,
-- no definition content. Member erasure therefore needs no plan entry, and
-- backups exclude the table (a recovered process republishes the full set
-- on boot, and a stale marker would clear on that first success). Cutover
-- chain only: `automation_commands` is cutover-created (0130) and the store
-- chain carries no automation tables. `IF NOT EXISTS` matches the S6
-- convention so a re-run never fails on DDL. Rollback is
-- `DROP TABLE IF EXISTS automation_command_registry_resync` (in-flight
-- pending state is lost; definitions and audit history survive, and the
-- next boot resync republishes the full set).
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS automation_command_registry_resync (
  guild_id     TEXT PRIMARY KEY,
  requested_at TIMESTAMPTZ NOT NULL
);
