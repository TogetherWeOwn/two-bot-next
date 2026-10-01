-- The `web_v1` contract: everything the website is allowed to read.
--
-- This file IS the contract, in executable form. docs/WEBSITE_CONTRACT.md
-- describes it in prose; if the two ever disagree, this file is what is
-- running and the doc is the bug.
--
-- Why this is not a migration
-- ---------------------------
-- A migration is immutable by rule (migrations/README.md). A contract view is
-- not: `v1.0 -> v1.1` adds a column to an existing view, and that is an edit to
-- this file, not a new file. So the tables live in migrations/ and the views
-- live here, applied with CREATE OR REPLACE by `npm run web:views`. Applying it
-- twice is a no-op.
--
-- That also gives us a free guard rail. CREATE OR REPLACE VIEW can append a
-- column but cannot rename, reorder, remove or retype one. So a change that
-- would silently break the website FAILS here, loudly, at deploy time - and a
-- failure in this file is the signal that the change needs a `web_v2` schema
-- rather than an edit. See docs/WEBSITE_CONTRACT.md §1.
--
-- Schema name
-- -----------
-- `web_v1` is substituted by the applier (src/store/webContract.ts) so a test
-- database can hold its own copy alongside other test schemas. In production
-- the substitution is the identity and this file is byte-for-byte what runs.
--
-- Table names are deliberately unqualified: they resolve through search_path
-- to whichever schema the bot's own tables live in, and Postgres freezes that
-- resolution into the view when it is created.

CREATE SCHEMA IF NOT EXISTS web_v1;

-- ---------------------------------------------------------------------------
-- Internal helpers. NOT part of the contract - do not call these from the
-- website. They are in this schema only because the website's role has USAGE
-- here and nowhere else.
-- ---------------------------------------------------------------------------

-- Parse an ISO-8601 text timestamp, returning NULL rather than raising if the
-- text is not a timestamp.
--
-- Every timestamp we store is written by an emitter as `new Date().toISOString()`
-- so this should never fire. It exists because the alternative failure mode is
-- unacceptable: one malformed row would turn `SELECT * FROM web_v1.live_counts`
-- into an error, and the landing page's whole job is to not do that. A bad row
-- should cost us one null, not the page.
CREATE OR REPLACE FUNCTION web_v1._ts(t text) RETURNS timestamptz
  LANGUAGE plpgsql IMMUTABLE STRICT AS $fn$
BEGIN
  RETURN t::timestamptz;
EXCEPTION WHEN others THEN
  RETURN NULL;
END;
$fn$;

-- The reverse direction, for the columns migration 0009 converted to real
-- timestamptz: render them back as exactly the ISO-8601 UTC string the
-- contract has always published. The website reads v1 columns as text and the
-- guarantee that they never retype outlives the storage improving underneath.
-- 'MS' always prints three digits, matching toISOString(), which wrote every
-- value the TEXT columns ever held.
CREATE OR REPLACE FUNCTION web_v1._iso(t timestamptz) RETURNS text
  LANGUAGE sql IMMUTABLE STRICT AS $fn$
  SELECT to_char(t AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
$fn$;

-- Same idea for the JSON metadata blob.
CREATE OR REPLACE FUNCTION web_v1._json(t text) RETURNS jsonb
  LANGUAGE plpgsql IMMUTABLE STRICT AS $fn$
BEGIN
  RETURN t::jsonb;
EXCEPTION WHEN others THEN
  RETURN NULL;
END;
$fn$;

-- ---------------------------------------------------------------------------
-- contract_meta - always exactly one row
-- ---------------------------------------------------------------------------
CREATE OR REPLACE VIEW web_v1.contract_meta AS
SELECT
  cm.contract_version,
  -- Prefer what the bot recorded; fall back to what the data says, so this is
  -- useful on day one, before anything has written the column.
  COALESCE(
    cm.guild_id,
    (SELECT g.guild_id FROM guild_counters g
      ORDER BY g.human_member_count_at DESC NULLS LAST, g.guild_id LIMIT 1),
    (SELECT mm.guild_id FROM members mm
      GROUP BY mm.guild_id ORDER BY count(*) DESC, mm.guild_id LIMIT 1)
  ) AS guild_id
FROM web_contract_meta cm;

-- ---------------------------------------------------------------------------
-- live_counts - always exactly one row, even when every value in it is null
-- ---------------------------------------------------------------------------
--
-- The staleness ceilings are applied HERE rather than by the collector, so a
-- stopped collector ages its own numbers out. If the ceiling lived in the
-- writer, a bot that died at 09:00 would still be publishing 09:00's count on
-- Tuesday.
--
--   online_count        goes null after 15 minutes - presence is volatile.
--   human_member_count  goes null after 24 hours   - membership barely moves.
--
-- The timestamps are returned unconditionally, including when the value beside
-- them has aged out. The website needs "we last knew at 09:00" to render the
-- degraded state honestly.
CREATE OR REPLACE VIEW web_v1.live_counts AS
SELECT
  CASE WHEN web_v1._ts(c.human_member_count_at) > now() - interval '24 hours'
       THEN c.human_member_count END AS human_member_count,
  CASE WHEN web_v1._ts(c.online_count_at) > now() - interval '15 minutes'
       THEN c.online_count END AS online_count,
  c.human_member_count_at AS counts_updated_at,
  c.online_count_at       AS online_updated_at
FROM (SELECT 1) AS anchor(one)
-- LEFT JOIN LATERAL, not a plain select: this is what makes "always exactly one
-- row" true when guild_counters is empty. An empty table must still answer.
LEFT JOIN LATERAL (
  SELECT g.* FROM guild_counters g
  ORDER BY g.human_member_count_at DESC NULLS LAST, g.guild_id
  LIMIT 1
) c ON TRUE;

-- ---------------------------------------------------------------------------
-- rank_counts - one row per rank, always all five, in ladder order
-- ---------------------------------------------------------------------------
CREATE OR REPLACE VIEW web_v1.rank_counts AS
SELECT
  l.rank_key,
  l.rank_label,
  l.rank_order,
  CASE WHEN web_v1._ts(s.snapshot_at) > now() - interval '24 hours'
       THEN s.member_count END AS member_count,
  CASE WHEN web_v1._ts(s.snapshot_at) > now() - interval '24 hours'
       THEN s.holders_count END AS holders_count,
  s.snapshot_at
FROM rank_ladder l
LEFT JOIN LATERAL (
  SELECT r.* FROM rank_snapshots r
  WHERE r.rank_key = l.rank_key
  ORDER BY r.snapshot_at DESC
  LIMIT 1
) s ON TRUE
ORDER BY l.rank_order;

-- ---------------------------------------------------------------------------
-- members - one row per human member, for profile pages
-- ---------------------------------------------------------------------------
--
-- Deliberately thin. No usernames or avatars: we do not store them
-- (docs/PRIVACY.md), so a profile page resolves the display name from Discord
-- at render time. No last_active_at, no message counts - see §4 of the doc.
--
-- People who have left stay in this view with is_current_member = false.
-- Dropping them would quietly flatter our retention numbers.
CREATE OR REPLACE VIEW web_v1.members AS
SELECT
  m.member_id,
  web_v1._iso(m.joined_at) AS joined_at,
  -- Precomputed so the website is not doing date maths, and so "tenure" means
  -- the same thing on every page.
  CASE WHEN m.joined_at IS NOT NULL
       THEN GREATEST(
              0,
              (now() AT TIME ZONE 'UTC')::date
                - (m.joined_at AT TIME ZONE 'UTC')::date
            )
       END AS tenure_days,
  mr.rank_key,
  (m.left_at IS NULL) AS is_current_member
FROM members m
LEFT JOIN member_ranks mr
       ON mr.guild_id = m.guild_id AND mr.member_id = m.member_id
WHERE NOT m.is_bot
  AND NOT EXISTS (
    SELECT 1
      FROM member_exclusions me
     WHERE me.guild_id = m.guild_id AND me.member_id = m.member_id
  );

-- ---------------------------------------------------------------------------
-- member_milestones - the event history a profile is allowed to show
-- ---------------------------------------------------------------------------
--
-- The whitelist is the entire point. Adding a milestone type is a contract
-- version bump and an edit to this WHERE clause, not something that happens by
-- accident because a new event type started being emitted. Otherwise "first
-- message at 14:02" ends up on a public page one day without anyone deciding
-- that it should.
--
-- 'rank_changed' is listed before anything emits it. That is intentional: the
-- rank collector can start emitting without this view - and therefore the
-- contract - needing a change.
CREATE OR REPLACE VIEW web_v1.member_milestones AS
SELECT
  e.member_id,
  CASE e.event_type
    WHEN 'member_join'  THEN 'joined'
    WHEN 'member_leave' THEN 'left'
    ELSE 'rank_changed'
  END AS milestone,
  web_v1._iso(e.occurred_at) AS occurred_at,
  CASE WHEN e.event_type = 'rank_changed'
       THEN web_v1._json(e.metadata) ->> 'rank_key' END AS detail
FROM events e
JOIN members m ON m.guild_id = e.guild_id AND m.member_id = e.member_id
WHERE e.member_id IS NOT NULL
  AND NOT m.is_bot
  AND e.event_type IN ('member_join', 'member_leave', 'rank_changed');

-- ---------------------------------------------------------------------------
-- upcoming_events / next_event
-- ---------------------------------------------------------------------------
--
-- Zero rows means there is no next event. That is the correct answer today -
-- the server has none scheduled - and the view will never invent a placeholder
-- to avoid an empty result. The designed empty state has to be reachable in
-- production, not only in a mockup.
CREATE OR REPLACE VIEW web_v1.upcoming_events AS
SELECT
  se.event_id,
  se.name,
  se.starts_at,
  se.channel_id,
  se.description
FROM scheduled_events se
WHERE se.status IN ('scheduled', 'active')
  AND web_v1._ts(se.starts_at) < now() + interval '90 days'
  -- An event that has already started is still the one to show, until Discord
  -- moves it off 'active'.
  AND (se.status = 'active' OR web_v1._ts(se.starts_at) >= now())
ORDER BY se.starts_at;

CREATE OR REPLACE VIEW web_v1.next_event AS
SELECT ue.* FROM web_v1.upcoming_events ue ORDER BY ue.starts_at LIMIT 1;

-- ---------------------------------------------------------------------------
-- funnel_daily / funnel_by_source - AUTHENTICATED STAFF PAGES ONLY
-- ---------------------------------------------------------------------------
--
-- These exist so the website can render the growth dashboard. They are not for
-- the public site: two-design/docs/CONTENT.md rules out growth framing on
-- public pages, and today these numbers are 0 joins in 30 days. Accurate and
-- self-defeating at the same time.
--
-- Aggregate and guild-level. No member ids leave through here.
CREATE OR REPLACE VIEW web_v1.funnel_daily AS
SELECT
  -- The same UTC day substr() cut off the ISO string before 0009.
  to_char(e.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS day,
  count(*) FILTER (WHERE e.event_type = 'member_join')          AS joins,
  count(*) FILTER (WHERE e.event_type = 'member_leave')         AS leaves,
  count(*) FILTER (WHERE e.event_type = 'first_message')        AS first_messages,
  count(*) FILTER (WHERE e.event_type = 'first_voice_session')  AS first_voice_sessions,
  count(*) FILTER (WHERE e.event_type = 'member_join')
    - count(*) FILTER (WHERE e.event_type = 'member_leave')     AS net_change
FROM events e
LEFT JOIN members m ON m.guild_id = e.guild_id AND m.member_id = e.member_id
WHERE NOT COALESCE(m.is_bot, FALSE)
GROUP BY to_char(e.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD');

-- `source` values are documented in docs/EVENTS.md. Render 'unknown' as itself;
-- never fold it into a real invite code.
CREATE OR REPLACE VIEW web_v1.funnel_by_source AS
SELECT
  to_char(e.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS day,
  e.source,
  count(*) AS joins
FROM events e
LEFT JOIN members m ON m.guild_id = e.guild_id AND m.member_id = e.member_id
WHERE e.event_type = 'member_join'
  AND NOT COALESCE(m.is_bot, FALSE)
GROUP BY to_char(e.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'), e.source;
