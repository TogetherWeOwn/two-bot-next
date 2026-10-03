-- 0407_invite_campaigns: target DDL for the go.two.gg redirect store (TOG-12415).
--
-- The Worker resolves `go.two.gg/<slug>` with exactly
--   SELECT slug, invite_code, label, disabled_at, created_at
--   FROM invite_campaigns WHERE slug = $1
-- (`wrangler/src/redirect-store.ts` LOOKUP_SQL) and records clicks into
-- `events`. Port of legacy 0006 (`crates/cutover/tests/fixtures/
-- legacy_migrations/0006_invite_campaigns.sql`): same slug PK + shape CHECK,
-- same NOT NULL invite_code + code index, same label/disabled_at/created_at
-- shape. Retiring a link still never breaks it: disabled rows keep resolving;
-- only the CLI listing hides them.
--
-- Typing follows the next-store convention (see 0405): timestamps are
-- TIMESTAMPTZ, not legacy TEXT. The Worker reads both timestamp columns as
-- opaque strings (`rowToCampaign`: `disabledAt: row.disabled_at`, where
-- node-postgres returns TIMESTAMPTZ as an ISO-8601 string), and the
-- REDIRECT_MAPPINGS_JSON snapshot parser accepts the same string shape, so
-- live rows and snapshot rows stay interchangeable. Legacy TEXT ISO-8601
-- values cast losslessly (`::timestamptz`); malformed legacy values fail the
-- copy transaction rather than being silently repaired, matching 0405.
--
-- Additive only: one new table plus its index, no changes to existing tables.
-- `IF NOT EXISTS` throughout, matching the S6 convention, so cross-running
-- the store and cutover chains against one schema stays safe in either order.
CREATE TABLE IF NOT EXISTS invite_campaigns (
  -- What goes in the URL. Lowercase, no surprises; isValidSlug() in
  -- wrangler/src/redirect.ts (and isValidSlug in legacy src/redirect/
  -- campaigns.ts) is the authority and is tested on both sides.
  slug          TEXT PRIMARY KEY,

  -- The Discord invite code this resolves to, WITHOUT the discord.gg/ prefix.
  -- Clicks are recorded as source `invite:<code>`, which is exactly the string
  -- joins are attributed to, so attribution lines the two up with no special
  -- case. Give each campaign its own code or the per-place breakdown — the
  -- entire point of this feature — collapses back into one number.
  invite_code   TEXT NOT NULL,

  -- Where we post it, for humans reading the report. 'Reddit r/MMORPG sidebar'.
  label         TEXT NOT NULL,

  -- Retiring a link must not break it. A disabled campaign still redirects, so
  -- a link already posted somewhere we cannot edit keeps working forever; it
  -- just stops being offered as a current campaign by the CLI listing.
  disabled_at   TIMESTAMPTZ,

  created_at    TIMESTAMPTZ NOT NULL DEFAULT date_trunc('milliseconds', now()),

  CONSTRAINT invite_campaigns_slug_shape
    CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,38}[a-z0-9]$'),
  CONSTRAINT invite_campaigns_code_nonempty
    CHECK (length(invite_code) > 0)
);

-- The report groups clicks by code; this is the lookup behind that join.
CREATE INDEX IF NOT EXISTS idx_invite_campaigns_code ON invite_campaigns (invite_code);
