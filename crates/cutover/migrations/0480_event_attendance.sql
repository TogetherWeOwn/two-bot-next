-- Dormant Event Echoes storage. The schema neither authorizes capture nor
-- backfills history from event instances, bindings or voice presence.
CREATE TABLE event_attendance_windows (
  guild_id   TEXT NOT NULL,
  event_key  TEXT NOT NULL,
  event_id   TEXT NOT NULL,
  instance_starts_at TIMESTAMPTZ NOT NULL,
  starts_at  TIMESTAMPTZ NOT NULL,
  ends_at    TIMESTAMPTZ NOT NULL,
  capture_stops TIMESTAMPTZ[] NOT NULL DEFAULT '{}',
  -- Last capture and mutation recency are distinct.
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  changed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (guild_id, event_key)
);

CREATE TABLE event_voice_sessions (
  id           BIGSERIAL PRIMARY KEY,
  guild_id     TEXT NOT NULL,
  member_id    TEXT NOT NULL,
  channel_id   TEXT NOT NULL,
  channel_name TEXT NOT NULL DEFAULT '',
  -- NULL denotes unknown observation order.
  name_observed_at TIMESTAMPTZ,
  changed_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
  is_bot       BOOLEAN NOT NULL DEFAULT FALSE,
  afk          BOOLEAN NOT NULL DEFAULT FALSE,
  joined_at    TIMESTAMPTZ NOT NULL,
  left_at      TIMESTAMPTZ,
  event_keys   TEXT[] NOT NULL DEFAULT '{}'
);

CREATE INDEX event_voice_sessions_open_idx
  ON event_voice_sessions (guild_id, member_id)
  WHERE left_at IS NULL;

CREATE INDEX event_voice_sessions_keys_idx
  ON event_voice_sessions USING GIN (event_keys);
