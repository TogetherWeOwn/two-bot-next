-- Legacy versions are copyable data, not safe CAS tokens: installed 0331
-- writers could issue unrecorded shadow tokens, and a read/setval reseed can
-- race standalone nextval. Keep applied migrations immutable; retire their
-- allocator from CAS rather than guessing a historical high-water mark.
LOCK TABLE guild_settings IN ACCESS EXCLUSIVE MODE;

-- A separate, never-reseeded namespace invalidates every formerly accepted
-- nonnegative executor token. Zero still denotes absence. Stay in JavaScript's
-- exact integer range; exhaustion refuses a write rather than cycling/reusing.
CREATE SEQUENCE guild_settings_cas_seq AS BIGINT
  INCREMENT BY -1 MINVALUE -9007199254740991 MAXVALUE -1 START -1 CACHE 1 NO CYCLE;

-- The volatile default backfills existing rows without settings DML, changing
-- values, audit or poll revision, or acquiring the revision lock. In particular,
-- a store writer already holding that lock can wait on this DDL without a cycle.
-- regclass defaults bind to this schema at installation, not caller search_path.
ALTER TABLE guild_settings
  ADD COLUMN cas_version BIGINT NOT NULL DEFAULT nextval('guild_settings_cas_seq');
ALTER TABLE guild_settings
  ALTER COLUMN version SET DEFAULT nextval('guild_settings_version_seq');

CREATE OR REPLACE FUNCTION guild_settings_assign_version() RETURNS TRIGGER AS $$
BEGIN
  -- Caller-supplied legacy versions survive copy/resume/hash verification.
  -- CAS ignores supplied tokens on every insert/update, including upserts.
  NEW.cas_version := pg_catalog.nextval(
    pg_catalog.format('%I.%I', TG_TABLE_SCHEMA, 'guild_settings_cas_seq')::pg_catalog.regclass
  );
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;
