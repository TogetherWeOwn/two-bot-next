-- Seasonal-decay idempotency watermark (TOG-22868, AC2 follow-up).
--
-- `apply_seasonal_decay` measures inactivity from `member_season_xp.updated_at`
-- (the last award) but previously rewrote only `seasonal_xp`: every later tick
-- re-applied all past decay on top of already-decayed values, and a rerun of
-- the same tick decayed the row again plus wrote a duplicate `source='decay'`
-- audit row. `decayed_weeks` records how many overdue weeks have already been
-- applied, so each tick applies only newly overdue weeks and reruns are
-- no-ops. Award upserts reset it to 0: new activity restarts the grace window.
--
-- Additive only: one new column with a backfill default on the 0100 table.
-- Every pre-existing row predates any decay run (the job has no production
-- caller yet), so backfilling 0 is exact. `IF NOT EXISTS` like the rest of
-- this chain.

ALTER TABLE member_season_xp
  ADD COLUMN IF NOT EXISTS decayed_weeks INTEGER NOT NULL DEFAULT 0;
