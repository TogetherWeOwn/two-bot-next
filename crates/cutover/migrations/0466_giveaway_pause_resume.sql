-- 0466_giveaway_pause_resume: pause-start marker for deadline extension.
--
-- Adds nullable `paused_at` to `giveaways` (set by pause, cleared by resume)
-- so resume can extend `ends_at` by the paused span instead of guessing it.
-- The draw rounds in `giveaway_draws` (`draw_no`, `announced_at` flow) and
-- the `giveaway_entries` PK are unchanged by this migration. Additive only:
-- `ADD COLUMN IF NOT EXISTS` so cross-running the store and cutover chains
-- against one schema stays safe in either order. Mirrors store 0465; global
-- migration numbers remain unique across both chains.
-- Bot schema range: 0001-0999. Database tests use test containers only.

ALTER TABLE giveaways ADD COLUMN IF NOT EXISTS paused_at TIMESTAMPTZ;
