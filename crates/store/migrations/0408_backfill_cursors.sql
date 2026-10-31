-- 0408_backfill_cursors: resumable per-family backfill cursors (TOG-22852 AC4).
--
-- A crash must never restart a window from zero: each endpoint family
-- (audit, members, history, event users, reactions, pins, bans, invites,
-- onboarding) checkpoints its Discord pagination cursor here, and the worker
-- resumes from the persisted cursor on boot. An untouched family has NO row
-- (cursorless = resume-from-start), never a synthesized zero/empty row.
--
-- Privacy fence (hard, same as event-store S1): rows carry ids, cursors and
-- timestamps only — family (TEXT PK), cursor (opaque Discord id TEXT),
-- updated_at (TIMESTAMPTZ). No message bodies, no custom-status text, no
-- payload columns. The CHECK below rejects empty/zero cursors at the
-- database level so a zero restart can never be persisted.
--
-- Additive only: one new table, no changes to existing tables, so a running
-- bot keeps working during the deploy (migrations/README.md rule 2).
-- `IF NOT EXISTS` throughout, matching the S6 convention, so cross-running
-- the store and cutover chains against one schema stays safe in either order.
CREATE TABLE IF NOT EXISTS backfill_cursors (
  family      TEXT PRIMARY KEY,
  cursor      TEXT NOT NULL CHECK (length(cursor) > 0 AND cursor <> '0'),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT date_trunc('milliseconds', now())
);
