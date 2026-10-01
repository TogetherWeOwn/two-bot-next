-- TOG-1649: scheduled-event RSVPs, raid/LFG sign-ups, and deduplicated feed relays.

CREATE TABLE IF NOT EXISTS event_rsvps (
  guild_id    TEXT NOT NULL,
  event_id    TEXT NOT NULL,
  user_id     TEXT NOT NULL,
  status      TEXT NOT NULL CHECK (status IN ('going', 'interested', 'declined')),
  responded_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (guild_id, event_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_event_rsvps_event
  ON event_rsvps (guild_id, event_id, status);

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

CREATE TABLE IF NOT EXISTS feed_relays (
  id              TEXT PRIMARY KEY,
  guild_id        TEXT NOT NULL,
  channel_id      TEXT NOT NULL,
  kind            TEXT NOT NULL CHECK (kind IN ('rss', 'youtube', 'twitch')),
  source          TEXT NOT NULL,
  enabled         BOOLEAN NOT NULL DEFAULT TRUE,
  last_checked_at TIMESTAMPTZ,
  created_by      TEXT NOT NULL,
  created_at      TIMESTAMPTZ NOT NULL,
  updated_at      TIMESTAMPTZ NOT NULL,
  UNIQUE (guild_id, channel_id, kind, source)
);

CREATE TABLE IF NOT EXISTS feed_deliveries (
  feed_id       TEXT NOT NULL REFERENCES feed_relays(id) ON DELETE CASCADE,
  item_key      TEXT NOT NULL,
  nonce         TEXT NOT NULL,
  state         TEXT NOT NULL CHECK (state IN ('pending', 'delivered')),
  message_id    TEXT,
  first_seen_at TIMESTAMPTZ NOT NULL,
  delivered_at  TIMESTAMPTZ,
  PRIMARY KEY (feed_id, item_key),
  UNIQUE (feed_id, nonce)
);

CREATE TABLE IF NOT EXISTS announcements_audit_log (
  id          TEXT PRIMARY KEY,
  guild_id    TEXT NOT NULL,
  actor_id    TEXT,
  action      TEXT NOT NULL,
  target_key  TEXT,
  outcome     TEXT NOT NULL,
  reason      TEXT,
  created_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_announcements_audit_guild_time
  ON announcements_audit_log (guild_id, created_at);
