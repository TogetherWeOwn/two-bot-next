-- 0450_scheduled_events_claim: per-guild epoch-claim gate for the scheduled-event mirror.
--
-- `replace_events` claims the fetch's `observed_at` inside its swap transaction
-- before DELETE: the first fetch for a guild inserts the row, a
-- newer-or-equal fetch moves the epoch forward, and a strictly older fetch
-- rolls the whole transaction back with a `StaleSnapshot` skip. A slow stale
-- fetch can therefore never commit after a newer tick deleted rows it still
-- carries. Additive only: one new table, no writes to the existing mirror, so
-- pre-existing snapshots read back exactly as before. `IF NOT EXISTS`
-- throughout, matching the S6 convention, so cross-running the store and
-- cutover chains against one schema stays safe in either order. Rollback is
-- `DROP TABLE IF EXISTS scheduled_events_claim`; the mirror stays readable.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS scheduled_events_claim (
  guild_id      TEXT NOT NULL,
  epoch         TEXT NOT NULL,
  PRIMARY KEY (guild_id)
);
