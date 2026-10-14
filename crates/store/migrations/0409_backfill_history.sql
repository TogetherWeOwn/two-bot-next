-- 0409_backfill_history: channel-history metadata catch-up (TOG-22852 slice 5, TOG-22950).
--
-- Slow, resumable catch-up of channel history metadata the bot did not
-- capture live. Paginates GET /channels/{channel}/messages with limit 1-100
-- and before/after/around cursors through the slice-1 rate-limit client;
-- resume checkpoints the floor id under family `history` in the slice-2
-- `backfill_cursors` table (0408).
--
-- Privacy fence (hard, same as event-store S1): metadata ONLY — message,
-- channel and author ids plus the send timestamp, and per-channel aggregate
-- counts. No message-body column, no custom-status text, no payload columns.
-- A fixture carrying a message body must fail if persisted (the acceptance
-- target's tripwire inserts into a `content` column that must not exist).
--
-- Additive only: two new tables, no changes to existing tables, so a running
-- bot keeps working during the deploy. `IF NOT EXISTS` throughout, matching
-- the S6 convention, so cross-running the store and cutover chains against
-- one schema stays safe in either order.
CREATE TABLE IF NOT EXISTS backfill_history_messages (
  message_id  TEXT PRIMARY KEY,
  channel_id  TEXT NOT NULL CHECK (length(channel_id) > 0),
  author_id   TEXT NOT NULL CHECK (length(author_id) > 0),
  sent_at     TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS backfill_history_counts (
  channel_id     TEXT PRIMARY KEY,
  message_count  BIGINT NOT NULL CHECK (message_count >= 0),
  updated_at     TIMESTAMPTZ NOT NULL DEFAULT date_trunc('milliseconds', now())
);

-- Newest-first paging per channel reads back the floor continuation fast.
CREATE INDEX IF NOT EXISTS backfill_history_messages_channel_idx
  ON backfill_history_messages (channel_id, message_id);
