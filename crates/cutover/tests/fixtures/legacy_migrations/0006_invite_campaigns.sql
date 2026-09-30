-- 0006_invite_campaigns: the tracked short links behind go.two.gg (TOG-116).
--
-- One row per place we post an invite. The redirect service resolves
-- `go.two.gg/<slug>` here, writes an `invite_click` event, and 302s to Discord.
--
-- Why a table and not a config file: adding a link is a community decision made
-- the moment someone is about to post somewhere, and it must not need a deploy
-- or an engineer. `npm run campaigns -- --add` writes a row and the link is
-- live on the next request.
--
-- Nothing member-level is stored here or by anything that reads it. A click row
-- carries the campaign and the time, and that is the whole record - see
-- docs/PRIVACY.md.
CREATE TABLE IF NOT EXISTS invite_campaigns (
  -- What goes in the URL. Lowercase, no surprises - see isValidSlug() in
  -- src/redirect/campaigns.ts, which is the authority and is tested.
  slug          TEXT PRIMARY KEY,

  -- The Discord invite code this resolves to, WITHOUT the discord.gg/ prefix.
  -- Clicks are recorded as source `invite:<code>`, which is exactly the string
  -- joins are attributed to, so scripts/attribution.ts lines the two up with no
  -- special case. Give each campaign its own code or the per-place breakdown -
  -- the entire point of this feature - collapses back into one number.
  invite_code   TEXT NOT NULL,

  -- Where we post it, for humans reading the report. 'Reddit r/MMORPG sidebar'.
  label         TEXT NOT NULL,

  -- Retiring a link must not break it. A disabled campaign still redirects, so
  -- a link already posted somewhere we cannot edit keeps working forever; it
  -- just stops being offered as a current campaign by the CLI listing.
  disabled_at   TEXT,

  created_at    TEXT NOT NULL,

  CONSTRAINT invite_campaigns_slug_shape
    CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,38}[a-z0-9]$'),
  CONSTRAINT invite_campaigns_code_nonempty
    CHECK (length(invite_code) > 0)
);

-- The report groups clicks by code; this is the lookup behind that join.
CREATE INDEX IF NOT EXISTS idx_invite_campaigns_code ON invite_campaigns (invite_code);
