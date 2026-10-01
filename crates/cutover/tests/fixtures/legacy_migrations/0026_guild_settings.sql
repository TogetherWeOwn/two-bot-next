-- TOG-3100 (TOG-3093 slice 1): the config store behind the admin dashboard.
--
-- Today every one of the ~60 feature settings is a Coolify environment edit plus
-- a redeploy, which is why the owner cannot change any of them. This table is
-- where those values move to.
--
-- Env stays the fallback permanently. A key with no row here reads from the
-- environment exactly as it does now, which is what makes this migration
-- additive on the day it ships and makes the undo path "stop writing rows".
--
-- Keyed per guild from the first day even though we run one guild: the column
-- is cheap to get right now and impossible to re-key later, and open-sourcing
-- means somebody else runs this against their own guild.

-- Monotonic and global, not per row. The bot polls `max(version)` every 15s and
-- refetches the whole (small) table when it moves; a per-row counter would make
-- "has anything changed at all" the one question the poll cannot answer
-- cheaply. Not LISTEN/NOTIFY: that needs a dedicated connection with its own
-- reconnect handling and docs/STACK.md sizes the pool at 5 deliberately.
CREATE SEQUENCE IF NOT EXISTS guild_settings_version_seq AS BIGINT START 1;

CREATE TABLE IF NOT EXISTS guild_settings (
  guild_id    TEXT NOT NULL,
  key         TEXT NOT NULL,
  value       JSONB NOT NULL,
  version     BIGINT NOT NULL,
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- Discord user id of the admin who saved it, or a named process for a
  -- machine write. Never null: an unattributed settings change is the one kind
  -- of row this table exists to make impossible.
  updated_by  TEXT NOT NULL,
  PRIMARY KEY (guild_id, key),
  -- The TWO_INTERNAL_* capability gates (src/internal/config.ts) decide what the
  -- *website* is allowed to make the bot do. A settings write that could widen
  -- that allowlist is a privilege-escalation primitive: anyone who compromises
  -- the website would grant themselves the rest of the allowlist, which is the
  -- exact property docs/INTERNAL_ACTIONS.md says the design exists to prevent.
  -- src/core/settings.ts refuses these keys; the constraint is here as well so
  -- that a bug, a psql session or a future writer cannot get past it. See the
  -- `admin-config-adr` document on TOG-3093 §2.4.
  CONSTRAINT guild_settings_no_internal_keys CHECK (key NOT LIKE 'TWO\_INTERNAL\_%')
);

-- The poll reads max(version); the refetch reads rows above the cached version.
CREATE INDEX IF NOT EXISTS idx_guild_settings_version ON guild_settings (version);

-- Append-only. Who changed the bot's behaviour, when, and what it was before is
-- the record that matters after an incident, so it is not the writer's choice
-- whether to keep it.
CREATE TABLE IF NOT EXISTS guild_settings_audit (
  id          BIGSERIAL PRIMARY KEY,
  guild_id    TEXT NOT NULL,
  key         TEXT NOT NULL,
  -- NULL old_value = the key had no row (it was reading from the environment).
  -- NULL new_value = the row was deleted, which is how a key is handed back to
  -- the environment.
  old_value   JSONB,
  new_value   JSONB,
  actor       TEXT NOT NULL,
  at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_guild_settings_audit_key
  ON guild_settings_audit (guild_id, key, at DESC);

-- "Append-only" as something the database enforces rather than something the
-- writer remembers. TRUNCATE does not fire row triggers, so the test fixtures
-- still reset cleanly; a stray UPDATE or DELETE does not.
CREATE OR REPLACE FUNCTION guild_settings_audit_append_only() RETURNS TRIGGER AS $$
BEGIN
  RAISE EXCEPTION 'guild_settings_audit is append-only (attempted %)', TG_OP;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_guild_settings_audit_append_only ON guild_settings_audit;
CREATE TRIGGER trg_guild_settings_audit_append_only
  BEFORE UPDATE OR DELETE ON guild_settings_audit
  FOR EACH ROW EXECUTE FUNCTION guild_settings_audit_append_only();
