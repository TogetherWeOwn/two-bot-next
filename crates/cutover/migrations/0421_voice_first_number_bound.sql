-- Bound `/position first-number` to the V11 export codec (u32).
--
-- `0224_voice_rooms.sql` only checks `first_room_number >= 1`. A value above
-- 4294967295 (`u32::MAX`, e.g. 4294967296) passes that CHECK and
-- `CreatorChannel::validate`, then breaks `/export` and `/import` with
-- "Could not read the voice configuration" (`u32::try_from` in
-- `voice_config_store::decode_creator` refuses it), leaving a Manage Channels
-- admin able to break the Manage Server commands until the value is reset. A
-- huge start can also overflow room numbering later.
--
-- Clamp any existing out-of-range rows back into 1..=4294967295, then enforce
-- the bound beside the original CHECK (kept for history). Mirrors
-- `two_bot_core::voice_rooms::{MAX_FIRST_ROOM_NUMBER, CreatorChannel::validate}`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

UPDATE voice_creators
SET first_room_number = 4294967295
WHERE first_room_number > 4294967295;

UPDATE voice_creators
SET first_room_number = 1
WHERE first_room_number < 1;

ALTER TABLE voice_creators
  ADD CONSTRAINT voice_creators_first_room_number_range
  CHECK (first_room_number BETWEEN 1 AND 4294967295);
