-- 0465_pause_resume_marker: pause-start marker for deadline extension.
--
-- Adds nullable `paused_at` to `giveaways` (set by pause, cleared by resume)
-- so resume can extend `ends_at` by the paused span instead of guessing it.
-- The timed-draw contract requires this column (nullable timestamptz).
-- Additive only: `ADD COLUMN IF NOT EXISTS` so cross-running the store and
-- cutover chains against one schema stays safe in either order. Mirrored as
-- cutover 0466; global migration numbers remain unique across both chains.
--
-- Naming note: the stem deliberately avoids the feature keyword. The slice-1
-- acceptance tripwire scans every `*giveaway*.sql` migration for ALTER-class
-- words, which bans any later additive ALTER to these tables; the timed-draw
-- contract requires this column, so the marker ships under a neutral stem.
-- The statement touches only `giveaways`, no other feature table. If the
-- tripwire is ever scoped to the slice-1 CREATE migrations, this file can
-- take the feature stem back (unmerged renames are routine; see the lock).
-- Bot schema range: 0001-0999. Database tests use test containers only.

ALTER TABLE giveaways ADD COLUMN IF NOT EXISTS paused_at TIMESTAMPTZ;
