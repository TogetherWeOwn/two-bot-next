-- 0455_afk_status: one AFK row per member for `/afk` status with mention replies.
--
-- `/afk [message]` upserts `(guild_id, user_id)` with the note (NULL when the
-- setter gave none) and the set time; a later message mentioning an AFK
-- member answers once with the note plus elapsed time, and the member's own
-- next message deletes the row. The per-guild row cap and the note bound
-- live in the `afk` core module and its `set_afk` statement, not in the
-- schema: the table is a plain keyed store with no sequences and no
-- secondary indexes (every access is by the full key or by the guild prefix
-- of it). `set_at` is `timestamptz`, matching the cutover-chain convention
-- and the rollback-delta measure cast; the store reads it back as epoch
-- millis for elapsed math. Additive only: one new table, no writes to
-- existing tables. `IF NOT EXISTS` throughout, matching the S6 convention,
-- so cross-running the store and cutover chains against one schema stays
-- safe in either order. Rollback is `DROP TABLE IF EXISTS afk_status`;
-- cleared statuses are lost and members re-set with `/afk`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS afk_status (
  guild_id TEXT NOT NULL,
  user_id  TEXT NOT NULL,
  note     TEXT NULL,
  set_at   TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (guild_id, user_id)
);
