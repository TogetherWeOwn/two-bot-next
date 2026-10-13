-- 0431_event_store_stage_interaction_audit_capture: append-only stage-instance /
-- interaction-observe / audit-log-entry capture for event-store S8.
--
-- Creates the append-only `event_store` table (if S1/S2/S3/S4/S5/S6 has not
-- created it yet) and adds the S8 columns: identity + timestamps (`event_type`,
-- `guild_id`, `channel_id`, `event_time`, `received_at`, `dedup_key`) plus the
-- stage-instance columns (`stage_instance_id`, `privacy_level` enum), the
-- interaction-observe columns (`interaction_id`, `interaction_type` enum,
-- `application_id`, `command_id`) and the audit-log-entry columns
-- (`audit_log_entry_id`, `action_type` enum, `actor_id`, `target_id`). Ids,
-- timestamps and enum fields only: no topic, token, changes, options, reason
-- or other free-text columns (see the ALLOWED_COLUMNS tripwire in
-- crates/discord/tests/event_store_s8.rs). Every insert runs
-- `ON CONFLICT (dedup_key) DO NOTHING` against the shared UNIQUE index below,
-- with `dedup_key` shaped as `<EVENT_TYPE>:<guild_id>:<entity_id>:<event_time>`.
-- The interaction capture is observe-only: it reads the dispatch fields and
-- never consumes them; the bearer token stays in the live dispatch for the
-- ACK path and is never stored.
--
-- Ordering: S8 may land before or after any sibling slice, so every step is
-- re-runnable (`IF NOT EXISTS`): the CREATE covers a missing table and the
-- ALTERs cover a table another slice created first with a narrower column set.
-- S2/S4 create `user_id NOT NULL`; the explicit `DROP NOT NULL` below relaxes
-- it so S8 stage/interaction/audit rows (which bind no user_id) insert cleanly.
-- Later slices must add their own columns with `ADD COLUMN IF NOT EXISTS` and
-- must reuse the shared dedup index name `idx_event_store_dedup_key` (via
-- `CREATE UNIQUE INDEX IF NOT EXISTS`) rather than adding a second UNIQUE
-- constraint, so the resume-safe key stays a single index.
--
-- Forward-compat nullability: `channel_id`, `user_id` and every S8 capture
-- column are NULLABLE so non-S8 rows (e.g. S1 message metadata, S2 member
-- deltas, S3 presence rows, which carry no stage/interaction/audit enums) can
-- share the table, and so S8 rows (which carry no member id) insert cleanly
-- when S2/S4 created the table first; `guild_id`/`event_time` stay NOT NULL
-- because every slice to date binds them. `user_id` is created NULLABLE here,
-- and the explicit `DROP NOT NULL` below relaxes it when an earlier slice
-- created it NOT NULL; without that step their NOT NULL would refuse S8 rows.
--
-- Erasure: rows are keyed by guild_id plus the capture identity
-- (stage_instance_id, interaction_id, audit_log_entry_id) with no other
-- identity, so per-guild purge and per-entity erasure are plain DELETEs on
-- those columns; retention windows stay with the retention slice, not this
-- migration. Roles/seeds: none; the table inherits the database's default
-- grants and carries no seed rows.
-- Rollback before merge (table is new and empty): `DROP TABLE IF EXISTS
-- event_store`. After rows accumulate there is no rollback that preserves
-- data; later slices only add columns.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_store (
  id                 BIGSERIAL PRIMARY KEY,
  event_type         TEXT NOT NULL,
  guild_id           TEXT NOT NULL,
  channel_id         TEXT,
  user_id            TEXT,
  stage_instance_id  TEXT,
  privacy_level      INTEGER,
  interaction_id     TEXT,
  interaction_type   INTEGER,
  application_id     TEXT,
  command_id         TEXT,
  audit_log_entry_id TEXT,
  action_type        INTEGER,
  actor_id           TEXT,
  target_id          TEXT,
  event_time         timestamptz NOT NULL,
  received_at        timestamptz NOT NULL DEFAULT NOW(),
  dedup_key          TEXT NOT NULL
);

ALTER TABLE event_store
  ADD COLUMN IF NOT EXISTS event_type         TEXT,
  ADD COLUMN IF NOT EXISTS guild_id           TEXT,
  ADD COLUMN IF NOT EXISTS channel_id         TEXT,
  ADD COLUMN IF NOT EXISTS user_id            TEXT,
  ADD COLUMN IF NOT EXISTS stage_instance_id  TEXT,
  ADD COLUMN IF NOT EXISTS privacy_level      INTEGER,
  ADD COLUMN IF NOT EXISTS interaction_id     TEXT,
  ADD COLUMN IF NOT EXISTS interaction_type   INTEGER,
  ADD COLUMN IF NOT EXISTS application_id     TEXT,
  ADD COLUMN IF NOT EXISTS command_id         TEXT,
  ADD COLUMN IF NOT EXISTS audit_log_entry_id TEXT,
  ADD COLUMN IF NOT EXISTS action_type        INTEGER,
  ADD COLUMN IF NOT EXISTS actor_id           TEXT,
  ADD COLUMN IF NOT EXISTS target_id          TEXT,
  ADD COLUMN IF NOT EXISTS event_time         timestamptz,
  ADD COLUMN IF NOT EXISTS received_at        timestamptz,
  ADD COLUMN IF NOT EXISTS dedup_key          TEXT;

-- S8 rows carry no member id; relax user_id when an earlier slice created it
-- NOT NULL. Safe when already nullable (no-op) and ordered after the ADD
-- COLUMN above so the column always exists here.
ALTER TABLE event_store ALTER COLUMN user_id DROP NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_event_store_dedup_key
  ON event_store (dedup_key);
