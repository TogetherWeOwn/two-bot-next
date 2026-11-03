-- 0471_event_attendance.sql: Event Echoes attendance log.
--
-- Two tables. `event_attendance_windows` persists one row per mapped event
-- (guild, event_key) with the upsert-confirmed start/end instants; the logger
-- and the keyless `event.attendance.read` verb derive the recording window
-- `[starts_at - 15 min, ends_at + 15 min]` from it. Windows are control-plane
-- records (no member identity): per-member erasure and guild purges do not
-- cover them, mirroring `scheduled_events`.
--
-- `event_voice_sessions` is the raw voice-session log: one row per
-- (member, channel, joined_at, left_at). Rows open only while a window is
-- open for the guild -- outside event windows nothing is recorded, never an
-- always-on voice log. A close finalizes the member's open row whenever the
-- leave/move arrives (even after the window closed), so minutes math needs no
-- repair pass; each closed row carries the union of windows overlapping
-- [joined_at, left_at] in `event_keys`, computed from the stored windows.
-- Bot accounts land in the log with `is_bot` (the read excludes them from
-- members and excludes bots plus AFK occupants from the peak); the AFK
-- channel at transition time lands in `afk`. `channel_name` is the latest
-- name the logger saw (refreshed at close, so renames converge).
--
-- Retention: raw sessions delete 90 days after `joined_at` through the
-- retention job. Per-member erasure runs through the member-erasure plan
-- (guild, member); the sessions table is guild-purge covered.
-- Snowflakes are TEXT, matching the cutover convention; no snowflake CHECKs,
-- so fixtures with short numeric ids insert cleanly.
--
-- Rollback: DROP TABLE IF EXISTS event_voice_sessions,
-- DROP TABLE IF EXISTS event_attendance_windows (constraints drop with them).
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_attendance_windows (
  guild_id   TEXT NOT NULL,
  event_key  TEXT NOT NULL,
  starts_at  TIMESTAMPTZ NOT NULL,
  ends_at    TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (guild_id, event_key)
);

CREATE TABLE IF NOT EXISTS event_voice_sessions (
  id           BIGSERIAL PRIMARY KEY,
  guild_id     TEXT NOT NULL,
  member_id    TEXT NOT NULL,
  channel_id   TEXT NOT NULL,
  channel_name TEXT NOT NULL DEFAULT '',
  is_bot       BOOLEAN NOT NULL DEFAULT FALSE,
  afk          BOOLEAN NOT NULL DEFAULT FALSE,
  joined_at    TIMESTAMPTZ NOT NULL,
  left_at      TIMESTAMPTZ,
  event_keys   TEXT[] NOT NULL DEFAULT '{}'
);

-- Open-row lookup for the close path: one member's unfinished sessions.
CREATE INDEX IF NOT EXISTS event_voice_sessions_open_idx
  ON event_voice_sessions (guild_id, member_id)
  WHERE left_at IS NULL;

-- Read path: sessions tagged with the requested event key.
CREATE INDEX IF NOT EXISTS event_voice_sessions_keys_idx
  ON event_voice_sessions USING GIN (event_keys);
