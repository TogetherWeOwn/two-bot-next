-- TOG-3052: temporary voice channels (join-to-create generator).
--
-- Postgres is the source of truth for which channels Owen created. Nothing
-- else is: the single hard invariant of this feature is that a channel with no
-- row here is never deleted, so category membership, naming conventions and
-- "the bot probably made it" are all deliberately unusable as evidence. See
-- src/tempVoice/service.ts.
--
-- `channel_id` is NULL for the window between reserving a row and Discord
-- acknowledging the create. A reservation that never gets a channel id is
-- dropped by the boot reconcile; it is never used to justify a delete.

CREATE TABLE IF NOT EXISTS temp_voice_channels (
  id              TEXT PRIMARY KEY,
  guild_id        TEXT NOT NULL,
  channel_id      TEXT,
  generator_id    TEXT NOT NULL,
  category_id     TEXT NOT NULL,
  owner_id        TEXT NOT NULL,
  created_by      TEXT NOT NULL,
  name            TEXT NOT NULL,
  created_at      TIMESTAMPTZ NOT NULL,
  last_renamed_at TIMESTAMPTZ,
  empty_since     TIMESTAMPTZ
);

-- One row per live Discord channel. The partial index leaves reservations
-- (channel_id IS NULL) out, so several can be in flight at once.
CREATE UNIQUE INDEX IF NOT EXISTS idx_temp_voice_channel
  ON temp_voice_channels (channel_id) WHERE channel_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_temp_voice_owner
  ON temp_voice_channels (guild_id, owner_id);
CREATE INDEX IF NOT EXISTS idx_temp_voice_guild_created
  ON temp_voice_channels (guild_id, created_at);

-- Per-user create cooldown. Separate from temp_voice_channels because the
-- cooldown has to outlive the channel it created.
CREATE TABLE IF NOT EXISTS temp_voice_creates (
  guild_id        TEXT NOT NULL,
  user_id         TEXT NOT NULL,
  last_created_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (guild_id, user_id)
);

CREATE TABLE IF NOT EXISTS temp_voice_audit (
  id          TEXT PRIMARY KEY,
  guild_id    TEXT NOT NULL,
  actor_id    TEXT,
  channel_id  TEXT,
  action      TEXT NOT NULL,
  outcome     TEXT NOT NULL,
  reason      TEXT,
  created_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_temp_voice_audit_guild_time
  ON temp_voice_audit (guild_id, created_at);
