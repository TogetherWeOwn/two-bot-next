-- 0467_lfg_close_refresh_retry: durable retry for failed close-message refreshes.
--
-- A close transition commits the closed row before its Discord PATCH runs, so
-- a failed edit (403, timeout, restart) leaves a closed post with stale
-- visible signup controls that the open-only lifecycle sweep never revisits.
-- These columns persist that pending close-message edit atomically with the
-- close itself: the lifecycle worker retries the exact saved message under
-- the existing runtime lock and send-admission fences, with durable backoff,
-- and clears pending only on a confirmed edit or a definitive Unknown
-- Message (Discord code 10008) outcome. Historical closed rows read back not
-- pending, so no bulk backfill occurs; erasure deletes the row and its
-- pending fact together.
--
-- Additive only: two NOT NULL columns with backfill defaults plus one
-- nullable next-eligible timestamp (NULL means eligible now), matching the
-- 0454 sticky-uncertainty precedent. `IF NOT EXISTS` throughout, matching
-- the S6 convention. Cutover chain only: `lfg_posts` is cutover-created
-- (0170) and the store chain carries no LFG tables. Rollback is
-- `ALTER TABLE lfg_posts DROP COLUMN IF EXISTS close_refresh_pending,
-- DROP COLUMN IF EXISTS close_refresh_attempts,
-- DROP COLUMN IF EXISTS close_refresh_next_at`
-- (in-flight retry state is lost; closures, signups and audit history
-- survive).
-- Bot schema range: 0001-0999. Database tests use test containers only.

ALTER TABLE IF EXISTS lfg_posts
    ADD COLUMN IF NOT EXISTS close_refresh_pending BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE IF EXISTS lfg_posts
    ADD COLUMN IF NOT EXISTS close_refresh_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE IF EXISTS lfg_posts
    ADD COLUMN IF NOT EXISTS close_refresh_next_at TIMESTAMPTZ;
