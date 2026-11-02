-- 0454_sticky_delivery_uncertainty: fence ambiguous sticky re-posts.
--
-- `claim_sticky_post` (0150) expires after 60 seconds, so a POST accepted by
-- Discord with a lost response, a 2xx with an unusable id, or a record
-- failure with unconfirmed orphan cleanup used to release the claim and let
-- the next accepted message send a fresh nonce -- a second sticky. Crash and
-- lease expiry must never implicitly clear that uncertainty.
--
-- This migration adds `delivery_uncertain`, mirroring
-- `moderation_scheduled_unbans.dispatch_uncertain`: the atomic claim arms
-- the fence, so a worker lost mid-POST leaves it behind, and only a
-- definitive outcome disarms it -- a confirmed record, a proven no-send
-- (explicit rejection, rate limit, local guard, pre-wire admission
-- refusal), confirmed orphan absence after a record failure, or an explicit
-- administrator reconfigure via `put_sticky` (the supported recovery path:
-- inspect the channel, delete any possible orphan, then run /sticky again
-- to reset the fence, or /sticky-remove plus /sticky to start over).
-- Obsolete completions (the claim moved on) never clear the fence; when
-- their orphan cleanup is unconfirmed they re-arm it instead. No lease,
-- restart or debounce transition clears it. Additive only: one NOT NULL
-- column with DEFAULT FALSE, so existing rows read back certain, matching
-- their pre-fence meaning. `IF NOT EXISTS` throughout, matching the S6
-- convention. Rollback is `ALTER TABLE sticky_messages DROP COLUMN IF
-- EXISTS delivery_uncertain` (in-flight uncertainty fences are lost;
-- posted stickies and audit history survive).
-- Bot schema range: 0001-0999. Database tests use test containers only.

ALTER TABLE IF EXISTS sticky_messages
    ADD COLUMN IF NOT EXISTS delivery_uncertain BOOLEAN NOT NULL DEFAULT FALSE;
