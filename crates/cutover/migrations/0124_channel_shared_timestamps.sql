-- Align the shared member/channel ledger without changing applied migration 0120.
-- Member migration 0112 performs the same conversion when that slice is installed.
-- Preserve rows; invalid historical timestamps fail loudly for reconciliation.
DO $$
DECLARE
  target RECORD;
BEGIN
  FOR target IN
    SELECT table_name, column_name FROM (VALUES
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
