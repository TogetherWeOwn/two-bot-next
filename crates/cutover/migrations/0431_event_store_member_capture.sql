-- 0431_event_store_member_capture: S2 member add/remove/update capture on the
-- append-only event_store table (parent scope TOG-22849 slice 2).
--
-- Stores one row per GUILD_MEMBER_ADD / REMOVE / UPDATE dispatch with ids,
-- timestamps and membership enum deltas only: event_type, guild_id, user_id,
-- event_time, received_at, dedup_key (UNIQUE, resume-safe idempotent key, see
-- below, `ON CONFLICT (dedup_key) DO NOTHING`) plus premium_since, the
-- nick_changed flag and the role-id array. No free-text columns: no nickname,
-- bio, about, username, body or content today or via this migration's column
-- list. The member slice keeps a nick_changed flag only, per the privacy
-- fence in docs/event-store.md (nickname under Never stored).
--
-- Resume-safe dedup keys, all first-wins against the single shared index
-- `idx_event_store_dedup_key`:
-- - ADD: `GUILD_MEMBER_ADD:<guild>:<user>:<joined_at>`. `joined_at` is a
--   payload timestamp, so a resume redelivery carries the same key while a
--   later rejoin carries a new one.
-- - REMOVE: `GUILD_MEMBER_REMOVE:<guild>:<user>:<session>:<seq>`. The remove
--   dispatch carries no timestamp, so the key uses the gateway envelope: the
--   READY session id plus the dispatch sequence number `s`, both provided by
--   Discord on every dispatch. A redelivered remove replays the same key and
--   inserts nothing; a later leave carries a new sequence number and a new
--   row. Keying on receipt time instead would duplicate every redelivery.
-- - UPDATE: `GUILD_MEMBER_UPDATE:<guild>:<user>:<session>:<seq>`, for the
--   same reason: the update dispatch carries no event timestamp.
-- `event_time` on timestamp-less dispatches stores receipt time (the writer's
-- best-known time), never a fabricated payload field.
--
-- Order-independent with the sibling slices (S1 core, S3 presence, S4 voice,
-- S5 message deltas, S6 threads, S8 stage/interaction/audit): every step is
-- re-runnable (`IF NOT EXISTS`). The shared dedup index name
-- `idx_event_store_dedup_key` is created once via
-- `CREATE UNIQUE INDEX IF NOT EXISTS`, never as an inline UNIQUE, so landing
-- after a sibling never leaves a second unique index on the column. Number
-- 0431 sits past the 0427-0430 sibling claims, so no two landed files share
-- a number. `id` is BIGSERIAL like the sibling slice tables. `roles` is
-- `NOT NULL DEFAULT '[]'::jsonb` so sibling rows that carry no role delta
-- still insert; the ALTER path backfills existing NULLs before enforcing
-- NOT NULL. No snowflake CHECK constraints on guild_id/user_id: fixtures and
-- early guilds carry short numeric ids, and a format check would refuse them.
--
-- Erasure: rows are keyed by guild_id + user_id with no other identity, so
-- per-guild purge and per-user erasure are plain DELETEs on those columns;
-- retention windows stay with the retention slice, not this migration.
-- Roles/seeds: none; the table inherits the database's default grants and
-- carries no seed rows.
-- Rollback: `ALTER TABLE event_store DROP COLUMN IF EXISTS roles`,
-- `DROP COLUMN IF EXISTS nick_changed`, `DROP COLUMN IF EXISTS premium_since`
-- (plus channel_id/message_id/id only when no other migration owns the
-- table); `DROP TABLE IF EXISTS event_store` solely when this migration
-- created the table and nothing else uses it.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_store (
    id BIGSERIAL PRIMARY KEY,
    event_type TEXT NOT NULL,
    guild_id TEXT NOT NULL,
    channel_id TEXT,
    message_id TEXT,
    user_id TEXT NOT NULL,
    event_time TIMESTAMPTZ NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    dedup_key TEXT NOT NULL,
    premium_since TIMESTAMPTZ,
    nick_changed BOOLEAN,
    roles JSONB NOT NULL DEFAULT '[]'::jsonb
);

ALTER TABLE event_store ADD COLUMN IF NOT EXISTS channel_id TEXT;
ALTER TABLE event_store ADD COLUMN IF NOT EXISTS message_id TEXT;
ALTER TABLE event_store ADD COLUMN IF NOT EXISTS premium_since TIMESTAMPTZ;
ALTER TABLE event_store ADD COLUMN IF NOT EXISTS nick_changed BOOLEAN;
ALTER TABLE event_store ADD COLUMN IF NOT EXISTS roles JSONB DEFAULT '[]'::jsonb;
UPDATE event_store SET roles = '[]'::jsonb WHERE roles IS NULL;
ALTER TABLE event_store ALTER COLUMN roles SET NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_event_store_dedup_key
    ON event_store (dedup_key);
