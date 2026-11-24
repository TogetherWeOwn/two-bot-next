-- Dormant live-hub ordering state. No row exists until the first reservation.
CREATE TABLE public.live_sequence_watermarks (
  guild_id TEXT PRIMARY KEY CHECK (
    guild_id ~ '^[1-9][0-9]{0,19}$'
    AND (length(guild_id) < 20 OR guild_id COLLATE "C" <= '18446744073709551615')
  ),
  seq BIGINT NOT NULL CHECK (seq BETWEEN 1 AND 9007199254740991)
);
