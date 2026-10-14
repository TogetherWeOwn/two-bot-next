-- 0440_stream_live_claims: durable go-live dedup ledger for Twitch
-- `stream.online` announcements.
--
-- One row per go-live keyed by `live_dedup_key` (sha256 hex of
-- `stream.online` + broadcaster + instant, see
-- `crates/core/src/stream_funnel.rs`). `claim_live` inserts with
-- `ON CONFLICT DO NOTHING`: an inserted row means this delivery owns the
-- post, a conflict means an EventSub retry or replay and is skipped. A send
-- that is known never to have reached Discord deletes its unposted row
-- (`release_live`) so the Twitch retry can claim again; a delivered row is
-- stamped, never deleted, so retries after a successful stamp cannot
-- double-post. An unposted claim does not expire: a crash after claim but
-- before send stays claimed and suppresses retries. The receiver must add
-- stale-claim recovery before using this ledger. No member identity:
-- `broadcaster_id` is a Twitch
-- platform id and `channel_id` names the announcements channel, so the
-- member erasure plan needs no entry (same shape as
-- `internal_discord_events`). No owned sequence: the dedup key is the
-- primary key, so the backup round-trip needs no DEFAULT-insert case.
-- Additive only: one new table plus one index, no changes to existing
-- tables. `IF NOT EXISTS` throughout, matching the S6 convention, so
-- cross-running the store and cutover chains against one schema stays safe
-- in either order. Rollback is `DROP INDEX IF EXISTS
-- idx_stream_live_claims_broadcaster_time` then `DROP TABLE IF EXISTS
-- stream_live_claims`.
-- Bot schema range: 0001-0999. Database tests use test containers only.

CREATE TABLE IF NOT EXISTS stream_live_claims (
    dedup_key TEXT PRIMARY KEY CHECK (dedup_key ~ '^[0-9a-f]{64}$'),
    broadcaster_id TEXT NOT NULL CHECK (broadcaster_id ~ '^[0-9]{1,32}$'),
    started_at TEXT NOT NULL CHECK (started_at <> '' AND char_length(started_at) <= 64),
    channel_id TEXT NOT NULL CHECK (channel_id ~ '^[0-9]{17,20}$'),
    content TEXT NOT NULL CHECK (content <> ''),
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    delivered_at TIMESTAMPTZ,
    message_id TEXT CHECK (message_id IS NULL OR message_id ~ '^[0-9]{17,20}$')
);

CREATE INDEX IF NOT EXISTS idx_stream_live_claims_broadcaster_time
  ON stream_live_claims (broadcaster_id, claimed_at);
