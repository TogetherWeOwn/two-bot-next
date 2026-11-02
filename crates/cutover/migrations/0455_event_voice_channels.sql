-- 0455_event_voice_channels: idempotent event voice channels + presence spans.
-- (Numbered 0455: the merged sticky delivery fence owns 0454.)
--
-- `event_voice_channels` keys one row per event instance
-- (`UNIQUE (event_id, instance_starts_at)`): the create path claims with
-- `INSERT ... ON CONFLICT DO NOTHING` holding `owner_tick_lease` before any
-- Discord call, so two workers or a double tick yield one channel. A
-- `starts_at` shift on an existing row updates that row (channel kept); a new
-- occurrence with a new start inserts a new row (series keying). `starts_at`
-- and `ends_at` are `timestamptz`; the create window is computed in the DB in
-- UTC and local times render on the web only. `ends_at` stays NULL when
-- Discord names no end time (the grace math defaults it, never PATCHed back).
-- Lifecycle vocabulary (no CHECK: later cards widen it without ALTER churn):
-- `creating` (lease held, channel maybe unbound), `orphaned` (channel 404'd,
-- lease cleared for one retry). `created_at` is the claim instant; staleness
-- compares `updated_at` (lease age), which every bind/clear/orphan write and
-- the ticker's re-lease moves. `UNIQUE (voice_channel_id)` is inline: one
-- Discord channel hosts at most one event instance, and the hot writer
-- lookup rides its backing index. NULL bindings stay repeatable.
-- `event_voice_presence` persists per-member join/leave spans for event
-- channels (`joined_at` on VOICE_STATE_UPDATE join/move-in, `left_at` on
-- leave; the 60s flush stays a repair-only backstop). Natural primary key,
-- no sequences. The inline `EXCLUDE ... WHERE (left_at IS NULL)` keeps at
-- most one open span per (event, instance, member) with no separate partial
-- index, so a join is `INSERT ... ON CONFLICT DO NOTHING` (which also
-- suppresses the exclusion) and a leave stamps the single open row; rejoins
-- open a second span and the handoff sums closed spans per instance.
-- Snowflakes are TEXT, matching the cutover convention; no snowflake CHECKs,
-- so fixtures with short numeric ids insert cleanly.
-- Erasure: presence rows carry `user_id` (covered by the member-erasure plan
-- via the scheduled-event mirror for guild scope); the channel row carries no
-- member identity (`owner_tick_lease` names a tick, not a member).
-- Additive only, `IF NOT EXISTS` throughout, constraints inline (no separate
-- CREATE INDEX statements): two new tables, no writes to existing tables.
-- Rollback is `DROP TABLE IF EXISTS event_voice_presence`,
-- `DROP TABLE IF EXISTS event_voice_channels` (constraints drop with them).
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS event_voice_channels (
  event_id           TEXT NOT NULL,
  instance_starts_at TIMESTAMPTZ NOT NULL,
  starts_at          TIMESTAMPTZ NOT NULL,
  ends_at            TIMESTAMPTZ,
  status             TEXT NOT NULL DEFAULT 'creating',
  owner_tick_lease   TEXT,
  voice_channel_id   TEXT,
  created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE (event_id, instance_starts_at),
  UNIQUE (voice_channel_id)
);

CREATE TABLE IF NOT EXISTS event_voice_presence (
  event_id  TEXT NOT NULL,
  instance  TIMESTAMPTZ NOT NULL,
  user_id   TEXT NOT NULL,
  joined_at TIMESTAMPTZ NOT NULL,
  left_at   TIMESTAMPTZ,
  PRIMARY KEY (event_id, instance, user_id, joined_at),
  CONSTRAINT event_voice_presence_one_open_span
    EXCLUDE USING btree (event_id WITH =, instance WITH =, user_id WITH =)
    WHERE (left_at IS NULL)
);
