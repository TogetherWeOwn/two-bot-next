-- 0014_ticket_safety: reserve opens, recover cleanup, and bound transcript retention.
DROP INDEX IF EXISTS idx_tickets_one_open;

ALTER TABLE tickets DROP CONSTRAINT IF EXISTS tickets_status_check;
ALTER TABLE tickets ALTER COLUMN channel_id DROP NOT NULL;
ALTER TABLE tickets ADD CONSTRAINT tickets_status_check
  CHECK (status IN ('creating', 'open', 'closing', 'cleanup_pending', 'closed'));
ALTER TABLE tickets ADD COLUMN IF NOT EXISTS closing_started_at TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_tickets_one_active
  ON tickets (guild_id, opener_id) WHERE status IN ('creating', 'open', 'closing', 'cleanup_pending');

ALTER TABLE ticket_transcripts ADD COLUMN IF NOT EXISTS purge_after TEXT;
UPDATE ticket_transcripts
  SET purge_after = to_char(
    created_at::timestamptz + INTERVAL '90 days',
    'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'
  )
  WHERE purge_after IS NULL;
ALTER TABLE ticket_transcripts ALTER COLUMN purge_after SET NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ticket_transcripts_purge ON ticket_transcripts (purge_after);
