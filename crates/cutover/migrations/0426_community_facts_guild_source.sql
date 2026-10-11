-- 0426_community_facts_guild_source: index the RSVP check-in capacity count.
--
-- `record_checkin` (crates/core/src/rsvp_store.rs) counts the occurrence's
-- facts with `SELECT COUNT(*) FROM community_facts WHERE guild_id = $1 AND
-- source = $2` while holding the per-occurrence advisory lock. No index
-- covered `source`: the only guild-prefixed indexes were
-- `(guild_id, occurred_at, id)` and
-- `(guild_id, event_type, classification, occurred_at)` (0160_rsvp), so every
-- check-in scanned the guild's whole fact history -- including one
-- `message_created` row per member message -- and held the lock for the
-- length of that scan. Check-in latency grew with message volume, not with
-- the 1000-row occurrence cap.
--
-- This additive index makes that count an index-only scan on
-- `(guild_id, source)`. Index-only, re-runnable (`IF NOT EXISTS`), no data
-- change, no retention change, no role/seed change; rollback is `DROP INDEX
-- IF EXISTS idx_community_facts_guild_source`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE INDEX IF NOT EXISTS idx_community_facts_guild_source
  ON community_facts (guild_id, source);
