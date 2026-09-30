-- Ticket lifecycle + transcripts: legacy 0013/0014 combined, same names/types.
CREATE TABLE IF NOT EXISTS tickets (
  id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  channel_id TEXT UNIQUE,
  opener_id TEXT NOT NULL,
  claimed_by TEXT,
  status TEXT NOT NULL CHECK (status IN ('creating', 'open', 'closing', 'cleanup_pending', 'closed')),
  created_at TEXT NOT NULL,
  closing_started_at TEXT,
  closed_at TEXT
);

-- Also upgrade databases that already contain the legacy 0013 table.
DROP INDEX IF EXISTS idx_tickets_one_open;
ALTER TABLE tickets DROP CONSTRAINT IF EXISTS tickets_status_check;
ALTER TABLE tickets ALTER COLUMN channel_id DROP NOT NULL;
ALTER TABLE tickets ADD COLUMN IF NOT EXISTS closing_started_at TEXT;
ALTER TABLE tickets ADD CONSTRAINT tickets_status_check
  CHECK (status IN ('creating', 'open', 'closing', 'cleanup_pending', 'closed'));
CREATE UNIQUE INDEX IF NOT EXISTS idx_tickets_one_active
  ON tickets (guild_id, opener_id) WHERE status IN ('creating', 'open', 'closing', 'cleanup_pending');
CREATE INDEX IF NOT EXISTS idx_tickets_guild_status ON tickets (guild_id, status, created_at);

CREATE TABLE IF NOT EXISTS ticket_transcripts (
  ticket_id TEXT PRIMARY KEY REFERENCES tickets(id) ON DELETE CASCADE,
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  opener_id TEXT NOT NULL,
  claimed_by TEXT,
  content TEXT NOT NULL,
  message_count INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  purge_after TEXT NOT NULL
);
ALTER TABLE ticket_transcripts ADD COLUMN IF NOT EXISTS purge_after TEXT;
UPDATE ticket_transcripts
  SET purge_after = to_char(
    (created_at::timestamptz + INTERVAL '90 days') AT TIME ZONE 'UTC',
    'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'
  )
  WHERE purge_after IS NULL;
ALTER TABLE ticket_transcripts ALTER COLUMN purge_after SET NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ticket_transcripts_guild ON ticket_transcripts (guild_id, created_at);
CREATE INDEX IF NOT EXISTS idx_ticket_transcripts_purge ON ticket_transcripts (purge_after);
