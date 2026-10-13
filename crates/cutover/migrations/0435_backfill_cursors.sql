-- 0435_backfill_cursors: cutover-chain target DDL for the resumable
-- per-family backfill cursors (TOG-22852 AC4; route slice TOG-22953).
--
-- Same table shape as the store chain's 0408; the two chains cross-run
-- additively (`IF NOT EXISTS`, no shared sequence), matching the
-- events/members/invite_snapshots precedent (store 0400 vs cutover 0001) and
-- the invite_campaigns precedent (store 0407 vs cutover 0411). The bot boots
-- with `Store::connect(url, true)` (skip migrations) and the staging and
-- production migrators embed only `crates/cutover/migrations`, so without
-- this mirror the `/ops/backfill` route (and the worker's
-- `put_backfill_cursor`) always answers 503 on a configured database with
-- "relation does not exist". Numbered 0435 to stay clear of main's 0433
-- privacy opt-out (cutover) and 0434 privacy opt-out (store), and of the
-- store chain's 0427 (`backfill_audit_entries`) on the stacked worker branch.
--
-- Privacy fence (hard, same as event-store S1): rows carry ids, cursors and
-- timestamps only — family (TEXT PK), cursor (opaque Discord id TEXT),
-- updated_at (TIMESTAMPTZ). No message bodies, no custom-status text, no
-- payload columns. The CHECK below rejects empty/zero cursors at the
-- database level so a zero restart can never be persisted.
--
-- Additive only: one new table, no changes to existing tables, so a running
-- bot keeps working during the deploy. `IF NOT EXISTS` throughout, matching
-- the S6 convention, so cross-running the store and cutover chains against
-- one schema stays safe in either order.
CREATE TABLE IF NOT EXISTS backfill_cursors (
  family      TEXT PRIMARY KEY,
  cursor      TEXT NOT NULL CHECK (length(cursor) > 0 AND cursor <> '0'),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT date_trunc('milliseconds', now())
);
