-- 0461_giveaways: timed-entry giveaways with a verifiable draw (slice 1).
--
-- Store-chain copy of cutover 0460: `giveaways` (definitions plus the
-- draw-ticker claim lease), `giveaway_entries` (one row per entrant, so the
-- PK collapses double-clicks and double POSTs), and `giveaway_draws` (one
-- row per draw round; rerolls insert, never update). The draw seed is
-- `sha256("{draw_nonce}:{draw_no}")` and `eligible_hash` commits to the
-- sorted eligible list, so any member can replay the draw from the stored
-- row. Both child tables carry a composite foreign key to `giveaways`
-- (`guild_id`, `giveaway_id`), so entries and draws stay bound to a live
-- definition: a parent delete with live children is refused instead of
-- orphaning them, and removals delete children first (`delete_giveaway`,
-- the erasure plan's creator branches, restore's parent-first order).
-- Additive only: one feature's new tables, no writes to existing
-- tables, `IF NOT EXISTS` throughout so cross-running the store and
-- cutover chains against one schema stays safe in either order. No
-- secondary indexes: a plain `CREATE INDEX` fails the squawk-strict lint
-- and `CONCURRENTLY` cannot run in the transactional migrators, so the
-- due-scan index is a follow-up with the reviewer. Rollback is
-- `DROP TABLE IF EXISTS giveaway_draws, giveaway_entries, giveaways`.
-- Numbered 0461 because cutover owns 0460; global migration numbers remain
-- unique across both chains.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS giveaways (
  guild_id          TEXT NOT NULL,
  id                TEXT NOT NULL,
  prize             TEXT NOT NULL,
  channel_id        TEXT NOT NULL,
  message_id        TEXT NOT NULL,
  created_by        TEXT NOT NULL,
  winners_count     INTEGER NOT NULL CHECK (winners_count >= 1 AND winners_count <= 100),
  ends_at           TIMESTAMPTZ NOT NULL,
  status            TEXT NOT NULL,
  requirements_json JSONB NOT NULL,
  max_entries       INTEGER NOT NULL CHECK (max_entries >= 1),
  draw_nonce        TEXT NOT NULL,
  claim_token       TEXT,
  claimed_at        TIMESTAMPTZ,
  announced_at      TIMESTAMPTZ,
  created_at        TIMESTAMPTZ NOT NULL,
  updated_at        TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (guild_id, id)
);

CREATE TABLE IF NOT EXISTS giveaway_entries (
  guild_id    TEXT NOT NULL,
  giveaway_id TEXT NOT NULL,
  user_id     TEXT NOT NULL,
  weight      INTEGER NOT NULL DEFAULT 1 CHECK (weight >= 1),
  entered_at  TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (guild_id, giveaway_id, user_id),
  FOREIGN KEY (guild_id, giveaway_id) REFERENCES giveaways (guild_id, id)
);

CREATE TABLE IF NOT EXISTS giveaway_draws (
  guild_id       TEXT NOT NULL,
  giveaway_id    TEXT NOT NULL,
  draw_no        INTEGER NOT NULL CHECK (draw_no >= 0),
  drawn_at       TIMESTAMPTZ NOT NULL,
  eligible_count INTEGER NOT NULL CHECK (eligible_count >= 0),
  eligible_hash  TEXT NOT NULL,
  seed           TEXT NOT NULL,
  winner_ids     JSONB NOT NULL,
  reroll_kind    TEXT,
  PRIMARY KEY (guild_id, giveaway_id, draw_no),
  FOREIGN KEY (guild_id, giveaway_id) REFERENCES giveaways (guild_id, id)
);
