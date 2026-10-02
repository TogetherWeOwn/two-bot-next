-- 0406_rollback_journal: durable cutover rollback journal and watermarks.
--
-- Cutover precondition (`docs/cutover.md`): a durable journal/watermark must
-- cover every affected table, deletes and the side-effect ledger —
-- "No complete rollback data path = NO-GO". The rollback window is
-- `[T_0, T_r]`, not the time since the last backup, so restore-to-`T_f`
-- alone would lose the whole watch window. This migration adds the data
-- path; the rollback procedure itself stays in the runbook, and wiring the
-- journal into every writer is follow-up work (TOG-12137 scope).
--
-- Design notes:
-- * One append-only row per write (insert/update/delete) with the affected
--   row's identity and a pre-write snapshot. Deletes carry no `updated_at`
--   a `WHERE updated_at > T_f` scan could find, so the pre-image is what
--   makes them reconcilable.
-- * Pre-images are TEXT JSON blobs, matching the funnel `events.metadata`
--   convention (byte-stable, `preserve_order` on write), not JSONB: the
--   journal is a restore source, not a query index.
-- * `rollback_watermarks` holds one restorable cursor per table. Writers
--   advance it with `GREATEST`, so the cursor is monotonic even under
--   concurrent inserts; the global restorable point is `MAX(id)`.
-- * Additive only: two new tables, no changes to existing tables, so a
--   running bot keeps working during the deploy (migrations/README.md
--   rule 2). `IF NOT EXISTS` throughout, matching the S6 convention.

CREATE TABLE IF NOT EXISTS rollback_journal (
  id            BIGSERIAL PRIMARY KEY,
  table_name    TEXT NOT NULL,
  row_identity  TEXT NOT NULL,
  op            TEXT NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
  pre_image     TEXT,
  recorded_at   timestamptz NOT NULL DEFAULT date_trunc('milliseconds', now())
);

CREATE INDEX IF NOT EXISTS idx_rollback_journal_table_seq
  ON rollback_journal (table_name, id);
CREATE INDEX IF NOT EXISTS idx_rollback_journal_row
  ON rollback_journal (table_name, row_identity);

CREATE TABLE IF NOT EXISTS rollback_watermarks (
  table_name      TEXT PRIMARY KEY,
  last_journal_id BIGINT NOT NULL DEFAULT 0 CHECK (last_journal_id >= 0),
  updated_at      timestamptz NOT NULL DEFAULT date_trunc('milliseconds', now())
);
