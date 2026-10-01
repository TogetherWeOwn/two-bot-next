-- 0013_tickets: durable private ticket lifecycle and audit transcripts.
CREATE TABLE IF NOT EXISTS tickets (
  id         TEXT PRIMARY KEY,
  guild_id   TEXT NOT NULL,
  channel_id TEXT NOT NULL UNIQUE,
  opener_id  TEXT NOT NULL,
  claimed_by TEXT,
  status     TEXT NOT NULL CHECK (status IN ('open', 'closed')),
  created_at TEXT NOT NULL,
  closed_at  TEXT
);

-- At most one open ticket per member per guild. Closed tickets remain history.
CREATE UNIQUE INDEX IF NOT EXISTS idx_tickets_one_open
  ON tickets (guild_id, opener_id) WHERE status = 'open';
CREATE INDEX IF NOT EXISTS idx_tickets_guild_status ON tickets (guild_id, status, created_at);

CREATE TABLE IF NOT EXISTS ticket_transcripts (
  ticket_id    TEXT PRIMARY KEY REFERENCES tickets(id) ON DELETE CASCADE,
  guild_id     TEXT NOT NULL,
  channel_id   TEXT NOT NULL,
  opener_id    TEXT NOT NULL,
  claimed_by   TEXT,
  content      TEXT NOT NULL,
  message_count INTEGER NOT NULL,
  created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ticket_transcripts_guild ON ticket_transcripts (guild_id, created_at);
