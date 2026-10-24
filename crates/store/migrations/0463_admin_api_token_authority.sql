-- 0463_admin_api_token_authority: named holders and bounded credential authority.
-- 0458/0459 are starboard; 0460/0461 are giveaways. Never rewrite
-- shipped 0456/0457. Mirrors cutover 0464; both chains may run on one schema.
-- SQLx runs this entire file in one transaction. Existing rows have no named
-- holder: SET NOT NULL must abort the upgrade, leaving data and DDL untouched.
-- No invented holder/default, deletion, expiry truncation or token issuance.
-- An operator must reconcile incompatible rows before retrying; this migration
-- does not authorize that action. Backout is binary-only: keep these constraints.

-- Freeze the catalog before inspection: a partial schema must not supply
-- fallback authority, even when empty or when its existing rows are valid.
LOCK TABLE admin_api_tokens IN ACCESS EXCLUSIVE MODE;
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_attribute
             WHERE attrelid = 'admin_api_tokens'::regclass
               AND attname IN ('holder_id', 'holder_kind')
               AND NOT attisdropped AND atthasdef) THEN
    RAISE EXCEPTION 'admin_api_tokens holder columns must not have defaults';
  END IF;
  -- ADD COLUMN IF NOT EXISTS retains existing types. Domains can supply a
  -- fallback without atthasdef, so accept only the exact plain TEXT contract.
  IF EXISTS (SELECT 1 FROM pg_attribute
             WHERE attrelid = 'admin_api_tokens'::regclass
               AND attname IN ('holder_id', 'holder_kind')
               AND NOT attisdropped
               AND (atttypid <> 'pg_catalog.text'::regtype OR atttypmod <> -1)) THEN
    RAISE EXCEPTION 'admin_api_tokens holder columns must use plain pg_catalog.text';
  END IF;
  -- Deterministic collations break equality ties by bytes. Nondeterministic
  -- equality can admit HUMAN as human; never silently rewrite that authority.
  IF EXISTS (SELECT 1 FROM pg_attribute a
             LEFT JOIN pg_collation c ON c.oid = a.attcollation
             WHERE a.attrelid = 'admin_api_tokens'::regclass
               AND a.attname IN ('holder_id', 'holder_kind')
               AND NOT a.attisdropped
               AND c.collisdeterministic IS DISTINCT FROM TRUE) THEN
    RAISE EXCEPTION 'admin_api_tokens holder columns must use deterministic collations';
  END IF;
END $$;

-- Parse canonical checks on this server without adding a TEMPORARY privilege
-- requirement. Use the registry's schema and existing schema CREATE privilege.
-- The empty reference is dropped before registry DDL; the file transaction also
-- rolls it back on refusal. A name collision refuses without replacing anything.
DO $$
DECLARE
  reference_name TEXT;
  reference_table REGCLASS;
BEGIN
  SELECT format('%I.admin_api_token_authority_contract', n.nspname)
    INTO reference_name
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.oid = 'admin_api_tokens'::regclass;
  EXECUTE format($contract$
    CREATE TABLE %s (
      holder_id TEXT,
      holder_kind TEXT,
      scopes TEXT[],
      issued_at TIMESTAMPTZ,
      expires_at TIMESTAMPTZ,
      CONSTRAINT admin_api_tokens_holder_named
        CHECK (holder_id ~ '[^[:space:]]'),
      CONSTRAINT admin_api_tokens_holder_kind_closed
        CHECK (holder_kind IN ('human', 'ai_agent')),
      CONSTRAINT admin_api_tokens_admin_human
        CHECK (holder_kind = 'human' OR NOT ('admin' = ANY(scopes))),
      CONSTRAINT admin_api_tokens_lifetime_bounded
        CHECK (isfinite(issued_at) AND isfinite(expires_at)
               AND expires_at > issued_at
               AND EXTRACT(EPOCH FROM expires_at) - EXTRACT(EPOCH FROM issued_at) <= 7776000)
    )
  $contract$, reference_name);
  reference_table := reference_name::regclass;
  IF EXISTS (
    SELECT 1 FROM pg_constraint actual
    JOIN pg_constraint canonical ON canonical.conname = actual.conname
    WHERE actual.conrelid = 'admin_api_tokens'::regclass
      AND canonical.conrelid = reference_table
      AND (actual.contype <> 'c' OR actual.connoinherit
           OR pg_get_expr(actual.conbin, actual.conrelid)
              IS DISTINCT FROM pg_get_expr(canonical.conbin, canonical.conrelid))
  ) THEN
    RAISE EXCEPTION 'admin_api_tokens authority constraints must match canonical inheritable CHECKs';
  END IF;
  EXECUTE format('DROP TABLE %s', reference_name);
END $$;

ALTER TABLE admin_api_tokens ADD COLUMN IF NOT EXISTS holder_id TEXT;
ALTER TABLE admin_api_tokens ADD COLUMN IF NOT EXISTS holder_kind TEXT;
ALTER TABLE admin_api_tokens ALTER COLUMN holder_id SET NOT NULL;
ALTER TABLE admin_api_tokens ALTER COLUMN holder_kind SET NOT NULL;

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint
                 WHERE conrelid = 'admin_api_tokens'::regclass
                   AND conname = 'admin_api_tokens_holder_named') THEN
    ALTER TABLE admin_api_tokens ADD CONSTRAINT admin_api_tokens_holder_named
      CHECK (holder_id ~ '[^[:space:]]') NOT VALID;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_constraint
                 WHERE conrelid = 'admin_api_tokens'::regclass
                   AND conname = 'admin_api_tokens_holder_kind_closed') THEN
    ALTER TABLE admin_api_tokens ADD CONSTRAINT admin_api_tokens_holder_kind_closed
      CHECK (holder_kind IN ('human', 'ai_agent')) NOT VALID;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_constraint
                 WHERE conrelid = 'admin_api_tokens'::regclass
                   AND conname = 'admin_api_tokens_admin_human') THEN
    ALTER TABLE admin_api_tokens ADD CONSTRAINT admin_api_tokens_admin_human
      CHECK (holder_kind = 'human' OR NOT ('admin' = ANY(scopes))) NOT VALID;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_constraint
                 WHERE conrelid = 'admin_api_tokens'::regclass
                   AND conname = 'admin_api_tokens_lifetime_bounded') THEN
    -- Epoch subtraction uses numeric seconds, not timezone-dependent calendar
    -- days or timestamp arithmetic that can overflow at the finite endpoints.
    ALTER TABLE admin_api_tokens ADD CONSTRAINT admin_api_tokens_lifetime_bounded
      CHECK (isfinite(issued_at) AND isfinite(expires_at)
             AND expires_at > issued_at
             AND EXTRACT(EPOCH FROM expires_at) - EXTRACT(EPOCH FROM issued_at) <= 7776000) NOT VALID;
  END IF;
END $$;

ALTER TABLE admin_api_tokens VALIDATE CONSTRAINT admin_api_tokens_holder_named;
ALTER TABLE admin_api_tokens VALIDATE CONSTRAINT admin_api_tokens_holder_kind_closed;
ALTER TABLE admin_api_tokens VALIDATE CONSTRAINT admin_api_tokens_admin_human;
ALTER TABLE admin_api_tokens VALIDATE CONSTRAINT admin_api_tokens_lifetime_bounded;
