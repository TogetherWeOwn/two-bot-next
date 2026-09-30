-- Convert legacy TEXT timestamps to timestamptz (TOG-10078, F2).
--
-- 0110 used CREATE TABLE IF NOT EXISTS, so deployments that already carried
-- the legacy ISO-TEXT timestamp columns kept them: the tables existed, the
-- DDL was a no-op, and neither 0110 nor 0111 converted the columns. Every
-- due-claim comparison then fails during SQL type checking
-- (`text <= timestamptz` has no operator, SQLSTATE 42883) — even when no
-- eligible jobs exist, and 0111's quarantine cannot fix a column type.
--
-- This migration converts every moderation timestamp column in place,
-- preserving every row: quarantine states, historical timestamps, reasons
-- and claim tokens. No acceptance is inferred: row states are untouched
-- (including 'quarantined' from 0111) and no schedule is activated.
--
-- Repeat-safe: each column converts only while it is still TEXT, so
-- re-runs and fresh timestamptz schemas are no-ops. A value that is not a
-- valid timestamp aborts the migration loudly instead of being silently
-- corrupted; the operator then reconciles that row explicitly.

DO $$
DECLARE
  target RECORD;
BEGIN
  FOR target IN
    SELECT table_name, column_name FROM (VALUES
      ('moderation_warnings', 'created_at'),
      ('moderation_scheduled_unbans', 'execute_at'),
      ('moderation_scheduled_unbans', 'created_at'),
      ('moderation_scheduled_unbans', 'completed_at'),
      ('moderation_scheduled_unbans', 'claimed_at'),
      ('moderation_audit', 'created_at'),
      ('moderation_idempotency', 'claimed_at'),
      ('moderation_idempotency', 'completed_at')
    ) AS columns_to_convert(table_name, column_name)
    WHERE EXISTS (
      SELECT 1 FROM information_schema.columns
      WHERE table_schema = current_schema()
        AND table_name = columns_to_convert.table_name
        AND column_name = columns_to_convert.column_name
        AND data_type = 'text'
    )
  LOOP
    EXECUTE format(
      'ALTER TABLE %I ALTER COLUMN %I TYPE timestamptz USING %I::timestamptz',
      target.table_name, target.column_name, target.column_name);
  END LOOP;
END $$;
