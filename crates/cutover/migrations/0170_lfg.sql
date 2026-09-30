-- LFG raid signups (TOG-10084).
--
-- Ports the LFG tables from legacy two-bot
-- `migrations/0024_announcements_feeds.sql`; table and column names are
-- preserved verbatim. The RSVP (`event_rsvps`) and feed (`feed_relays`,
-- `feed_deliveries`) tables from that same legacy file belong to TOG-10083 /
-- TOG-10085, and `announcements_audit_log` lands with the S6 store port
-- (TOG-9811) — this file carries only the three LFG tables.

CREATE TABLE IF NOT EXISTS lfg_posts (
  id          TEXT PRIMARY KEY,
  guild_id    TEXT NOT NULL,
  channel_id  TEXT NOT NULL,
  message_id  TEXT,
  title       TEXT NOT NULL,
  starts_at   TIMESTAMPTZ NOT NULL,
  status      TEXT NOT NULL CHECK (status IN ('open', 'closed')),
  created_by  TEXT NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL,
  closed_at   TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS lfg_roles (
  lfg_id      TEXT NOT NULL REFERENCES lfg_posts(id) ON DELETE CASCADE,
  role_key    TEXT NOT NULL,
  label       TEXT NOT NULL,
  slots       INTEGER NOT NULL CHECK (slots BETWEEN 1 AND 99),
  position    INTEGER NOT NULL,
  PRIMARY KEY (lfg_id, role_key),
  UNIQUE (lfg_id, position)
);

CREATE TABLE IF NOT EXISTS lfg_signups (
  lfg_id      TEXT NOT NULL REFERENCES lfg_posts(id) ON DELETE CASCADE,
  user_id     TEXT NOT NULL,
  role_key    TEXT NOT NULL,
  joined_at   TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (lfg_id, user_id),
  FOREIGN KEY (lfg_id, role_key) REFERENCES lfg_roles(lfg_id, role_key) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_lfg_signups_role
  ON lfg_signups (lfg_id, role_key, joined_at);
