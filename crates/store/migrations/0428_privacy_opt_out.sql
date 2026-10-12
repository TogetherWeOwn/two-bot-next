-- 0428_privacy_opt_out: per-user opt-out suppression, per-guild analytics
-- toggles, and the opt-out audit receipt.
--
-- A member's opt-out installs a guild-scoped suppression row
-- (`privacy_opt_outs`): future per-user event/member writes for that
-- (guild_id, member_id) pair are dropped at the store seam, while aggregate
-- counters over other members keep counting. Suppression is never global:
-- the primary key is the pair, so other guilds and members are unaffected.
-- Removing the row (opt-back-in) restores persistence for that pair only.
--
-- `guild_analytics_settings` holds one row per guild with independent
-- presence / activity / voice ingestion switches. A missing row means every
-- category is enabled, which preserves today's behavior for guilds that
-- never touch the toggles. Only the analytics families named in
-- `docs/privacy.md` consult these switches; membership lifecycle rows
-- (joins, leaves) still persist for safety/moderation.
--
-- `privacy_opt_out_audit` keeps exactly one receipt row per opt-out/purge:
-- the guild/member scope plus the operator actor label and the database
-- timestamp. The actor column carries operator attribution only, never the
-- subject's content; the service always supplies an explicit actor, and the
-- column default covers receipt inserts that only name the scope. Suppression
-- and audit rows are control-plane tombstones: member erasure allowlists
-- them (see `member_erasure_plan.json`) instead of deleting them, because
-- deleting suppression would silently resume persistence the member refused.
--
-- Additive only: three new tables plus one index, no changes to existing
-- tables. `IF NOT EXISTS` throughout, matching the S6 convention, so
-- cross-running the store and cutover chains against one schema stays safe
-- in either order. Rollback is `DROP INDEX IF EXISTS
-- idx_privacy_opt_out_audit_scope` then `DROP TABLE IF EXISTS
-- privacy_opt_out_audit`, `guild_analytics_settings`, `privacy_opt_outs`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS privacy_opt_outs (
  guild_id    TEXT NOT NULL,
  member_id   TEXT NOT NULL,
  opted_out_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (guild_id, member_id)
);

CREATE TABLE IF NOT EXISTS guild_analytics_settings (
  guild_id         TEXT PRIMARY KEY,
  presence_enabled BOOLEAN NOT NULL DEFAULT TRUE,
  activity_enabled BOOLEAN NOT NULL DEFAULT TRUE,
  voice_enabled    BOOLEAN NOT NULL DEFAULT TRUE,
  updated_at       TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE IF NOT EXISTS privacy_opt_out_audit (
  id          BIGSERIAL PRIMARY KEY,
  guild_id    TEXT NOT NULL,
  member_id   TEXT NOT NULL,
  actor       TEXT NOT NULL DEFAULT 'operator',
  opted_out_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX IF NOT EXISTS idx_privacy_opt_out_audit_scope
  ON privacy_opt_out_audit (guild_id, member_id);
