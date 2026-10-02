-- 0411_invite_campaigns: legacy-copy target DDL for the go.two.gg redirect
-- store (TOG-12415). Same table shape as the S6 runtime chain's 0407; the two
-- chains cross-run additively (`IF NOT EXISTS`, no shared sequence), matching
-- the events/members/invite_snapshots precedent (store 0400 vs cutover 0001).
-- The copier itself never applies migrations: provision this target
-- separately. Timestamps are TIMESTAMPTZ per the next-store convention (0405);
-- the legacy-copy engine casts source TEXT (`s.created_at::timestamptz`),
-- which is lossless for ISO-8601 and fail-closed otherwise.
CREATE TABLE IF NOT EXISTS invite_campaigns (
  slug          TEXT PRIMARY KEY,
  invite_code   TEXT NOT NULL,
  label         TEXT NOT NULL,
  disabled_at   TIMESTAMPTZ,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT date_trunc('milliseconds', now()),

  CONSTRAINT invite_campaigns_slug_shape
    CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,38}[a-z0-9]$'),
  CONSTRAINT invite_campaigns_code_nonempty
    CHECK (length(invite_code) > 0)
);

CREATE INDEX IF NOT EXISTS idx_invite_campaigns_code ON invite_campaigns (invite_code);
