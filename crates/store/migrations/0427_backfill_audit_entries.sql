-- 0427_backfill_audit_entries: audit-log 45d catch-up (backfill worker slice 3).
--
-- Slow, resumable catch-up of guild audit-log entries the bot did not
-- capture live. Pages GET /guilds/{guild}/audit-logs with limit 1-100 and
-- before/after cursors through the slice-1 rate-limit client; resume
-- checkpoints the floor id under family `audit` in the slice-2
-- `backfill_cursors` table (0408).
--
-- Privacy fence (hard, same as event-store S1): ids/timestamps/enums ONLY —
-- guild and entry ids, the action-type enum, the acting/target user ids and
-- the occurrence timestamp. No reason text, no options/body columns, no
-- payload columns. A write naming one fails at the database level (the
-- acceptance tripwire inserts into no such column because none exists).
--
-- Additive only: one new table plus its read index, no changes to existing
-- tables, so a running bot keeps working during the deploy. `IF NOT EXISTS`
-- throughout, matching the S6 convention, so cross-running the store and
-- cutover chains against one schema stays safe in either order.
CREATE TABLE IF NOT EXISTS backfill_audit_entries (
  guild_id    TEXT NOT NULL CHECK (length(guild_id) > 0),
  entry_id    TEXT NOT NULL CHECK (length(entry_id) > 0),
  action_type INTEGER NOT NULL,
  user_id     TEXT,
  target_id   TEXT,
  occurred_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (guild_id, entry_id)
);

-- Newest-first paging per guild reads back the floor continuation fast.
CREATE INDEX IF NOT EXISTS backfill_audit_entries_guild_idx
  ON backfill_audit_entries (guild_id, occurred_at DESC, entry_id DESC);
