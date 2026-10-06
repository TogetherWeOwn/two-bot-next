-- Durable room-create admission (voice parity: rolling create burst limits).
--
-- One row per accepted create attempt, written under a per-guild advisory
-- lock before any Discord call. Rows are never deleted when a room is deleted
-- or the create is rolled back: the burst window and the per-member cooldown
-- count accepted reservations, not live rooms, so deletion, rollback and
-- restart refund nothing. `channel_id` and `settled_at` only track the
-- in-flight state that holds a cap slot (`settled_at IS NULL`): set on
-- success (room bound) or on rollback (no room). Cap counts add in-flight
-- reservations to `voice_rooms`. A hold never expires by age: an unknown
-- create outcome or a failed compensation keeps its slot until evidence
-- (a guarded delete or a 404) permits settlement. `channel_id` is bound as
-- soon as Discord returns the channel, before the room row is written, so a
-- restarted worker can find a created channel whose persist failed and whose
-- compensation delete was refused (see `PgRoomStore::claim_create`).
--
-- Snowflakes are TEXT with the canonical-id CHECK used by 0229; no foreign
-- keys, as with 0224/0229. Burst and cooldown history is bounded by the
-- window (60 s) and the cooldown ceiling (3600 s) at read time; no retention
-- job is part of this migration.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS voice_create_reservations (
  -- Text UUID, as 0205: no sequence to restore, and one id never repeats.
  id         TEXT        PRIMARY KEY DEFAULT pg_catalog.gen_random_uuid()::text,
  guild_id   TEXT        NOT NULL
    CHECK (guild_id ~ '^[0-9]{1,20}$' AND guild_id <> '0' AND guild_id NOT LIKE '0%'),
  user_id    TEXT        NOT NULL
    CHECK (user_id ~ '^[0-9]{1,20}$' AND user_id <> '0' AND user_id NOT LIKE '0%'),
  created_at timestamptz NOT NULL,
  channel_id TEXT
    CHECK (channel_id IS NULL OR (channel_id ~ '^[0-9]{1,20}$' AND channel_id <> '0' AND channel_id NOT LIKE '0%')),
  settled_at timestamptz
);

-- Window scan for the guild burst count.
CREATE INDEX IF NOT EXISTS idx_voice_create_reservations_guild_time
  ON voice_create_reservations (guild_id, created_at);

-- Last accepted create for the per-member cooldown and member burst count.
CREATE INDEX IF NOT EXISTS idx_voice_create_reservations_user_time
  ON voice_create_reservations (guild_id, user_id, created_at);
