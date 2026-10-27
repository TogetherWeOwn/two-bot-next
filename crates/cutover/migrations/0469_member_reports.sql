-- 0469_member_reports: storage for private member reports (slice 1).
--
-- Storage only: nothing writes these tables yet. Intake, permissions, staff
-- delivery and snapshot fetching are later slices. One member_reports row is
-- one report GROUP per reported message; further reporters coalesce into it.
--
-- Bounds are enforced by the schema, not by callers:
--   * at most one open/claimed group per (guild, message); closed groups never
--     absorb a new report;
--   * exactly one single-message snapshot per group, content at most 4000
--     characters, attachment metadata at most 10 entries and 16 KiB;
--   * at most 10 persisted reporter identities per group (report_count may
--     keep growing as an aggregate without storing more identities);
--   * reasons at most 1000 characters.
--
-- Lifecycle: a closed group is deleted 90 days after closed_at by the
-- retention runner (children follow via ON DELETE CASCADE). Erasure keeps the
-- accountability tombstone (id, guild, message, reported member, count,
-- disposition, timestamps, snapshot_status) but removes the erased party's
-- identity and evidence; see member_erasure_plan.json for column-specific
-- reasons. These tables are excluded from backups on purpose, so a restore can
-- never resurrect erased or expired evidence.
--
-- Additive: new tables with guarded indexes. Cutover
-- chain only. Rollback is `DROP TABLE report_reporters, report_snapshots,
-- member_reports; DROP FUNCTION report_reporters_cap()` (no earlier binary
-- reads them).
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE member_reports (
    id                 TEXT PRIMARY KEY CHECK (id ~ '^[0-9]{1,20}$'),
    guild_id           TEXT NOT NULL CHECK (guild_id ~ '^[0-9]{1,20}$'),
    channel_id         TEXT CHECK (channel_id ~ '^[0-9]{1,20}$'),
    message_id         TEXT NOT NULL CHECK (message_id ~ '^[0-9]{1,20}$'),
    reported_member_id TEXT NOT NULL CHECK (reported_member_id ~ '^[0-9]{1,20}$'),
    -- First reporter. NULL once that member is erased.
    reporter_id        TEXT CHECK (reporter_id ~ '^[0-9]{1,20}$'),
    -- Staff member who claimed the group; retained as accountability.
    claimer_id         TEXT CHECK (claimer_id ~ '^[0-9]{1,20}$'),
    report_count       INTEGER NOT NULL DEFAULT 1 CHECK (report_count >= 1),
    reason             TEXT NOT NULL DEFAULT '' CHECK (char_length(reason) <= 1000),
    snapshot_status    TEXT NOT NULL
        CHECK (snapshot_status IN ('present', 'missing-deleted', 'missing-unreadable')),
    status             TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'claimed', 'closed')),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at          TIMESTAMPTZ,
    disposition        TEXT CHECK (char_length(disposition) BETWEEN 1 AND 64),
    CONSTRAINT member_reports_closed_shape CHECK (
        (status = 'closed') = (closed_at IS NOT NULL)
        AND (status <> 'closed' OR disposition IS NOT NULL)
    )
);

-- One active group per guild and message; closed groups are excluded.
CREATE UNIQUE INDEX IF NOT EXISTS member_reports_active_message
    ON member_reports (guild_id, message_id) WHERE status IN ('open', 'claimed');

-- Retention walks closed groups by closure time.
CREATE INDEX IF NOT EXISTS member_reports_closed_at
    ON member_reports (closed_at) WHERE status = 'closed';

-- Member-scoped erasure lookups (reporter and reported author).
CREATE INDEX IF NOT EXISTS member_reports_guild_reporter
    ON member_reports (guild_id, reporter_id) WHERE reporter_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS member_reports_guild_reported
    ON member_reports (guild_id, reported_member_id);

CREATE TABLE report_snapshots (
    report_id           TEXT PRIMARY KEY REFERENCES member_reports (id) ON DELETE CASCADE,
    -- NULL for missing-deleted / missing-unreadable: nothing was fetched.
    author_id           TEXT CHECK (author_id ~ '^[0-9]{1,20}$'),
    author_bot          BOOLEAN,
    message_created_at  TIMESTAMPTZ,
    content             TEXT CHECK (char_length(content) <= 4000),
    attachments         JSONB NOT NULL DEFAULT '[]'::jsonb
        CHECK (jsonb_typeof(attachments) = 'array'
               AND jsonb_array_length(attachments) <= 10
               AND octet_length(attachments::text) <= 16384),
    attachment_overflow BOOLEAN NOT NULL DEFAULT false
);

CREATE INDEX IF NOT EXISTS report_snapshots_author
    ON report_snapshots (author_id) WHERE author_id IS NOT NULL;

CREATE TABLE report_reporters (
    report_id   TEXT NOT NULL REFERENCES member_reports (id) ON DELETE CASCADE,
    reporter_id TEXT NOT NULL CHECK (reporter_id ~ '^[0-9]{1,20}$'),
    reported_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (report_id, reporter_id)
);

CREATE INDEX IF NOT EXISTS report_reporters_reporter
    ON report_reporters (reporter_id);

-- At most ten persisted identities per group. The parent row lock serializes
-- concurrent coalescing inserts, so two racing reporters cannot both take the
-- tenth-plus slot.
--
-- Deliberately not a database_role_matrix.sql function row: matrix functions
-- are strict in the bootstrap role phase, which replays the 29-version staging
-- ledger and so cannot know a function created after it. Its privileges are
-- instead closed here: no role but the owner (the migrator) can execute it, and
-- a trigger fires without EXECUTE on its function.
CREATE FUNCTION report_reporters_cap() RETURNS trigger AS $$
BEGIN
    PERFORM 1 FROM member_reports WHERE id = NEW.report_id FOR UPDATE;
    IF (SELECT count(*) FROM report_reporters WHERE report_id = NEW.report_id) >= 10 THEN
        RAISE EXCEPTION 'report reporter identities are bounded to 10 per group'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$ LANGUAGE plpgsql;

REVOKE ALL ON FUNCTION report_reporters_cap() FROM PUBLIC;

CREATE TRIGGER report_reporters_cap
    BEFORE INSERT ON report_reporters
    FOR EACH ROW EXECUTE FUNCTION report_reporters_cap();
