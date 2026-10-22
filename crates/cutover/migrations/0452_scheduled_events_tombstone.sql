-- 0452_scheduled_events_tombstone: removal markers for the scheduled-event mirror.
--
-- `replace_events` records one row per mirror row its DELETE removes, stamped
-- with the tick observed_at in deleted_at, so an equal-stamp stale replay
-- cannot resurrect a removed id. Each claiming tick also clears tombstones
-- older than 24 h and trims each guild to its newest 1,000 rows, oldest
-- first. Additive only: one new table, no writes to the existing mirror, so
-- pre-existing snapshots read back exactly as before. No covering index: the
-- guild primary-key prefix serves the per-guild scans and the table holds at
-- most 1,000 rows per guild. `IF NOT EXISTS` throughout, matching the S6
-- convention, so cross-running the store and cutover chains against one
-- schema stays safe in either order. Rollback drops the table; the mirror
-- stays readable. Bot schema range: 0001-0999. Database tests use test
-- containers only.

CREATE TABLE IF NOT EXISTS scheduled_events_tombstone (
  guild_id      TEXT NOT NULL,
  event_id      TEXT NOT NULL,
  deleted_at    TEXT NOT NULL,
  PRIMARY KEY (guild_id, event_id)
);
