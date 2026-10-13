-- 0436_member_catchup: cutover-chain target DDL for the member-list catch-up
-- worker slice (TOG-22941, TOG-22852 slice 8 companion).
--
-- The catch-up worker persists per-user rows (`member_catchup_members`) and
-- its per-guild resume cursor (`member_catchup_cursors`); without this
-- migration every write on a migrated database fails with
-- "relation does not exist" (the tables previously existed only in the
-- acceptance fixture DDL). The members frontier is additionally checkpointed
-- into `backfill_cursors` under the `members` family (0435 mirror of store
-- 0408), which is what `GET /ops/backfill/progress` reads — the per-guild
-- cursor table alone never reaches the route.
--
-- Privacy fence (hard, same as event-store S1): rows carry ids, timestamps
-- and enums only — guild/member snowflakes, `joined_at`, `pending`, `is_bot`,
-- opaque `after` cursors and write instants. No message bodies, no
-- custom-status text, no payload columns. The `after_cursor` CHECK rejects
-- empty/zero cursors at the database level so a zero restart can never be
-- persisted (same bar as the `backfill_cursors` CHECK).
--
-- Additive only: two new tables, no changes to existing tables, so a running
-- bot keeps working during the deploy. `IF NOT EXISTS` throughout, matching
-- the S6 convention, so cross-running the store and cutover chains against
-- one schema stays safe in either order.
CREATE TABLE IF NOT EXISTS member_catchup_members (
  guild_id    TEXT NOT NULL,
  member_id   TEXT NOT NULL,
  joined_at   TIMESTAMPTZ,
  pending     BOOLEAN,
  is_bot      BOOLEAN NOT NULL DEFAULT FALSE,
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT date_trunc('milliseconds', now()),
  PRIMARY KEY (guild_id, member_id)
);
CREATE TABLE IF NOT EXISTS member_catchup_cursors (
  guild_id     TEXT PRIMARY KEY,
  after_cursor TEXT NOT NULL CHECK (length(after_cursor) > 0 AND after_cursor <> '0'),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT date_trunc('milliseconds', now())
);
