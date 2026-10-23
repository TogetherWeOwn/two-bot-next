-- 0459_bump_reminders: one durable pending bump reminder per guild.
--
-- The bot upserts the guild row on each bumper-bot success embed (a fresh
-- bump replaces the pending reminder instead of stacking), and lazily claims
-- the row once its cooldown elapses: a single atomic UPDATE flips one due,
-- unfired row to fired and returns its channel, so each arming pings at most
-- once across sweeps and restarts. Additive only: one new table, no writes
-- to existing tables. `IF NOT EXISTS` throughout, matching the S6
-- convention, so cross-running the store and cutover chains against one
-- schema stays safe in either order. Rollback is
-- `DROP TABLE IF EXISTS bump_reminders`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS bump_reminders (
  guild_id      TEXT NOT NULL,
  channel_id    TEXT NOT NULL,
  fires_at      TIMESTAMPTZ NOT NULL,
  fired         BOOLEAN NOT NULL DEFAULT FALSE,
  PRIMARY KEY (guild_id)
);
