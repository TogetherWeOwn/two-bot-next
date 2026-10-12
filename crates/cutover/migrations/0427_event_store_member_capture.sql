-- 0427_event_store_member_capture: S2 member add/remove/update capture on the
-- append-only event_store table (parent scope TOG-22849 slice 2).
--
-- Stores one row per GUILD_MEMBER_ADD / REMOVE / UPDATE dispatch with ids,
-- timestamps and membership enum deltas only: event_type, guild_id, user_id,
-- event_time, received_at, dedup_key (UNIQUE, resume-safe idempotent key
-- `GUILD_MEMBER_<ADD|REMOVE|UPDATE>:<guild>:<user>:<event_time>`,
-- `ON CONFLICT (dedup_key) DO NOTHING`) plus premium_since, nick and the
-- role-id array. No free-text columns: no bio, about, username, body or
-- content today or via this migration's column list.
--
-- Order-independent with the S1 core migration (unmerged at the time of
-- writing): when no event_store table exists this creates it with the
-- S1+S2 union schema (S1's channel_id/message_id included as nullable so
-- either slice's fixtures insert cleanly); when S1 landed first the CREATE
-- is a no-op and the ADD COLUMNs attach the S2 deltas. S1's migration must
-- likewise use IF NOT EXISTS / ADD COLUMN IF NOT EXISTS for its columns.
-- No snowflake CHECK constraints on guild_id/user_id: fixtures and early
-- guilds carry short numeric ids, and a format check would refuse them.
--
-- Erasure: rows are keyed by guild_id + user_id with no other identity, so
-- per-guild purge and per-user erasure are plain DELETEs on those columns;
-- retention windows stay with the retention slice, not this migration.
-- Roles/seeds: none; the table inherits the database's default grants and
-- carries no seed rows.
-- Rollback: `ALTER TABLE event_store DROP COLUMN IF EXISTS roles`,
-- `DROP COLUMN IF EXISTS nick`, `DROP COLUMN IF EXISTS premium_since`
-- (plus channel_id/message_id/id only when no other migration owns the
-- table); `DROP TABLE IF EXISTS event_store` solely when this migration
-- created the table and nothing else uses it.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_store (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_type TEXT NOT NULL,
    guild_id TEXT NOT NULL,
    channel_id TEXT,
    message_id TEXT,
    user_id TEXT NOT NULL,
    event_time TIMESTAMPTZ NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    dedup_key TEXT NOT NULL UNIQUE,
    premium_since TIMESTAMPTZ,
    nick TEXT,
    roles JSONB NOT NULL DEFAULT '[]'::jsonb
);

ALTER TABLE event_store ADD COLUMN IF NOT EXISTS channel_id TEXT;
ALTER TABLE event_store ADD COLUMN IF NOT EXISTS message_id TEXT;
ALTER TABLE event_store ADD COLUMN IF NOT EXISTS premium_since TIMESTAMPTZ;
ALTER TABLE event_store ADD COLUMN IF NOT EXISTS nick TEXT;
ALTER TABLE event_store ADD COLUMN IF NOT EXISTS roles JSONB DEFAULT '[]'::jsonb;
